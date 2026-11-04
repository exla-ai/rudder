//! Off-App first source preparation from the exact already accepted Git HEAD.
//! The caller retains its actual project/source owner and checks Journal HEAD
//! around this operation. This builds immutable bytes, not Journal authority.
//! A failed call leaves its private directories/staging/pins for reconciliation.
use anyhow::{ensure, Context, Result};
use jj_lib::{backend::CommitId, git_backend::GitBackend, object_id::ObjectId as _,
    op_store::RefTarget, ref_name::{RefName, WorkspaceName}, repo::{ReadonlyRepo, Repo},
    settings::UserSettings, workspace::Workspace};
use rudder_engine::{execution::finalization_budget::FinalizationBudget,
    protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef},
    snapshot::{decode_environment, RunnableRoot, SourceV2}, store::LocalStore,
    vfs::HardlinkTopology, workspace::WorkspaceProfile};
use serde::Serialize;
use std::{fs, path::Path, sync::Arc, time::Duration};
use crate::{accepted_git_import::{import_expected_git_head_profile_retaining,RetainedGitImport,GitImportLimits,GitHistoryProfile},
    projection::{materialize_source, verify_versioned_source, VerifiedSourceProjection},
    projection_io::ProjectionBudget, publication::INTEGRATION_BOOKMARK,
    seed::prepare_internal_metadata_template, vfs_provider::SourceProjectionPolicy,
    CommitIdentity, GitCommitId, JjOperationId};

pub struct InitialSourceRequest<'a> {
    pub source_git: &'a Path,
    /// Caller-created empty private directory in its managed integration store.
    pub private_repository: &'a Path,
    /// Must not exist; a failed export is never reopened or merged.
    pub seed_destination: &'a Path,
    pub expected_head: &'a GitCommitId,
    pub environment: &'a ObjectRef,
    pub settings: &'a UserSettings,
}

/// Actual live jj/store/verifier owners, deliberately not Serde or Clone.
/// Keep this owner off App, including errors/results rejected by the App.
pub struct PreparedInitialSource {
    _workspace:Option<RetainedGitImport>,
    repository: Arc<ReadonlyRepo>,
    pub baseline: CommitIdentity,
    pub operation: JjOperationId,
    pub runnable_root: ObjectRef,
    pub source_root: ObjectRef,
    pub seed_root: ObjectRef,
    pub runnable_pin: String,
    pub seed_pin: String,
    pub verified: Arc<VerifiedSourceProjection>,
    seed_cache: Arc<crate::seed::RetainedSeedCasCache>,
    #[cfg(target_os="linux")] runnable_backing:Option<Arc<rudder_engine::store::backed_retention::BackedPinOwner>>,
    #[cfg(target_os="linux")] runnable_dependency:Option<Arc<rudder_engine::store::environment_dependencies::EnvironmentDependentPinOwner>>,
    #[cfg(target_os="linux")] source_parent:Option<Arc<rudder_engine::store::environment_dependencies::RetainedSourceParent>>,
}
impl PreparedInitialSource {
    #[cfg(target_os="linux")]
    pub fn runnable_backing(&self)->Option<&Arc<rudder_engine::store::backed_retention::BackedPinOwner>>{self.runnable_backing.as_ref()}
    #[cfg(target_os="linux")]
    pub fn runnable_dependency(&self)->Option<&Arc<rudder_engine::store::environment_dependencies::EnvironmentDependentPinOwner>>{self.runnable_dependency.as_ref()}
    #[cfg(target_os="linux")]
    pub fn runnable_inputs(&self)->Option<rudder_engine::store::environment_dependencies::RetainedRunnableInputs>{use rudder_engine::store::environment_dependencies::RetainedRunnableInputs;
        self.runnable_dependency.as_ref().map(|p|RetainedRunnableInputs::Environment(p.clone())).or_else(||self.runnable_backing.as_ref().map(|p|RetainedRunnableInputs::Flat(p.clone())))}
    /// Shared completed immutable DATA fact from the actual flat publication.
    #[cfg(target_os="linux")]
    pub fn source_parent(&self)->Option<&Arc<rudder_engine::store::environment_dependencies::RetainedSourceParent>>{self.source_parent.as_ref()}
    pub fn repository(&self) -> &Arc<ReadonlyRepo> { &self.repository }
    pub fn seed_cache(&self)->&Arc<crate::seed::RetainedSeedCasCache>{&self.seed_cache}
}

fn phase(budget: &FinalizationBudget, check_owner: &mut impl FnMut() -> Result<()>) -> Result<()> {
    budget.remaining()?; check_owner()?; budget.remaining()?; Ok(())
}
fn projection_budget(budget: &FinalizationBudget) -> Result<ProjectionBudget> {
    let remaining = budget.remaining()?.as_millis().min(30_000) as u64;
    ensure!(remaining > 0, "initial source budget expired");
    Ok(ProjectionBudget { max_elapsed_ms: remaining, ..ProjectionBudget::default() })
}
fn staged<T: Serialize>(store: &LocalStore, stage: &str, kind: ObjectKind, value: &T) -> Result<ObjectRef> {
    let bytes = canonical_json(value)?;
    let reference = ObjectRef::from_bytes(kind, &bytes);
    store.receive_staged(stage, &reference, &bytes)?; Ok(reference)
}

/// One attempt only. The input source is never initialized, checked out,
/// rewritten or copied as mutable Git metadata. Git object preimages, original
/// accepted commit ID and its complete reachable history are imported exactly.
/// A caller may not retry into either destination after an uncertain failure.
pub async fn prepare_initial_source(store: LocalStore, request: InitialSourceRequest<'_>,
    original: &FinalizationBudget, check_owner: impl FnMut() -> Result<()>) -> Result<PreparedInitialSource> {
    let mut unused=None;
    prepare_initial_source_profile_retaining(store,request,crate::seed::SeedProfile::SmallV1,
        &mut unused,original,Arc::new(std::sync::atomic::AtomicBool::new(false)),check_owner).await
}
/// Explicit V2. `paged` belongs to the original native preparation capsule and
/// must remain outside callback/catch scopes across every error/late ACK.
pub async fn prepare_initial_source_profile_retaining(store:LocalStore,request:InitialSourceRequest<'_>,
    profile:crate::seed::SeedProfile,paged:&mut Option<crate::seed::PagedSeedGeneration>,
    original:&FinalizationBudget,cancelled:Arc<std::sync::atomic::AtomicBool>,mut validate_owner:impl FnMut()->Result<()>)->Result<PreparedInitialSource>{
    let mut import=None;
    let mut prepared=prepare_initial_source_profile_with_import_retaining(store,request,profile,paged,&mut import,original,cancelled,validate_owner).await?;prepared._workspace=import.take();Ok(prepared)
}
/// Production callers retain BOTH carriers outside the operation/catch. A failed
/// import keeps its actual partial pack/index/keep FDs, progress and no ACK.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_initial_source_profile_with_import_retaining(store:LocalStore,request:InitialSourceRequest<'_>,
 profile:crate::seed::SeedProfile,paged:&mut Option<crate::seed::PagedSeedGeneration>,import:&mut Option<RetainedGitImport>,
 original:&FinalizationBudget,cancelled:Arc<std::sync::atomic::AtomicBool>,validate_owner:impl FnMut()->Result<()>)->Result<PreparedInitialSource>{
    prepare_initial_source_history_with_import_retaining(store,request,profile,GitHistoryProfile::RawObjectsV1,paged,import,original,cancelled,validate_owner).await
}
/// Explicit representation; SAME original source/seed/environment carriers and
/// clock. Existing APIs always select RawObjectsV1, including old V2 callers.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_initial_source_history_with_import_retaining(store:LocalStore,request:InitialSourceRequest<'_>,
 profile:crate::seed::SeedProfile,history:GitHistoryProfile,paged:&mut Option<crate::seed::PagedSeedGeneration>,import:&mut Option<RetainedGitImport>,
 original:&FinalizationBudget,cancelled:Arc<std::sync::atomic::AtomicBool>,validate_owner:impl FnMut()->Result<()>)->Result<PreparedInitialSource>{
    prepare_initial_source_with_retention(store,request,profile,history,paged,import,original,cancelled,InitialEnvironmentRetention::Local,validate_owner).await
}
enum InitialEnvironmentRetention<'a>{Local,#[cfg(target_os="linux")]Backed(&'a mut backed::BackedInitialEnvironment),
    #[cfg(not(target_os="linux"))] _Lifetime(std::marker::PhantomData<&'a ()>)}
#[allow(clippy::too_many_arguments)]
async fn prepare_initial_source_with_retention(store:LocalStore,request:InitialSourceRequest<'_>,
 profile:crate::seed::SeedProfile,history:GitHistoryProfile,paged:&mut Option<crate::seed::PagedSeedGeneration>,import:&mut Option<RetainedGitImport>,
 original:&FinalizationBudget,cancelled:Arc<std::sync::atomic::AtomicBool>,mut retention:InitialEnvironmentRetention<'_>,mut validate_owner:impl FnMut()->Result<()>)->Result<PreparedInitialSource>{
    let original_cancel=cancelled.clone();
    let mut check_owner=||->Result<()> {
        if original_cancel.load(std::sync::atomic::Ordering::Acquire){original.cancel();anyhow::bail!("initial source original cancellation");}
        validate_owner()?;
        if original_cancel.load(std::sync::atomic::Ordering::Acquire){original.cancel();anyhow::bail!("initial source late original cancellation");}Ok(())
    };
    phase(original, &mut check_owner)?;
    request.expected_head.validate()?;
    request.environment.validate()?;
    ensure!(request.environment.kind == ObjectKind::EnvironmentRoot,
        "initial source requires an explicit retained environment");
    match &mut retention {
        InitialEnvironmentRetention::Local=>{decode_environment(&store.read_object(request.environment)?)?;
            ensure!(store.retention()?.pins.values().any(|r|r==request.environment),"initial environment has no actual local retention");}
        #[cfg(target_os="linux")] InitialEnvironmentRetention::Backed(owner)=>owner.check_environment_retaining(request.environment,original,&cancelled)?,
        #[cfg(not(target_os="linux"))] InitialEnvironmentRetention::_Lifetime(_)=>anyhow::bail!("unsupported environment retention"),
    }
    ensure!(request.private_repository.is_absolute()
        && request.private_repository.canonicalize()? == request.private_repository
        && fs::symlink_metadata(request.private_repository)?.is_dir()
        && fs::read_dir(request.private_repository)?.next().is_none(),
        "initial private repository must be a fresh empty real directory");
    #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(request.private_repository)?;
        ensure!(metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "initial private repository must belong exclusively to this owner");
    }
    ensure!(request.seed_destination.is_absolute()
        && fs::symlink_metadata(request.seed_destination).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "initial seed destination must be absent");
    let seed_parent = request.seed_destination.parent().context("initial seed has no parent")?;
    ensure!(seed_parent.canonicalize()? == seed_parent
        && !request.seed_destination.starts_with(request.private_repository),
        "initial seed destination aliases or overlaps the private repository");
    let source = gix::open(request.source_git).context("open exact accepted Git object store")?;
    let original_git_directory = source.common_dir().canonicalize()?;
    ensure!(!request.private_repository.starts_with(&original_git_directory)
        && !request.seed_destination.starts_with(&original_git_directory),
        "initial outputs cannot write into the original Git metadata");
    phase(original, &mut check_owner)?;
    let (workspace, repo) = Workspace::init_internal_git(request.settings, request.private_repository).await?;
    RetainedGitImport::stage_initial_repository_profile_retaining(import,workspace,repo.clone(),request.expected_head,original,cancelled.clone(),history)?;
    phase(original, &mut check_owner)?;
    let backend = repo.store().backend_impl::<GitBackend>().context("initial repository must use internal Git")?;
    import_expected_git_head_profile_retaining(import,&source,backend,request.expected_head,GitImportLimits::default(),history,original,cancelled.clone(),&mut check_owner)?;
    phase(original, &mut check_owner)?;
    let commit = repo.store().get_commit(&CommitId::new(hex::decode(&request.expected_head.0)?))?;
    let baseline = CommitIdentity::from_jj(&commit);
    ensure!(baseline.commit == *request.expected_head && baseline.tree.terms.len() == 1,
        "initial accepted commit differs or contains an unresolved tree");
    let mut transaction = repo.start_transaction();
    transaction.repo_mut().add_head(&commit).await?;
    transaction.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK), RefTarget::normal(commit.id().clone()));
    transaction.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(), commit.id().clone())?;
    let repo = transaction.commit("initialize Rudder accepted baseline").await?;
    phase(original, &mut check_owner)?;
    // jj's view/WC commit does not advance a bare Git HEAD. The private
    // seed must also expose the exact accepted baseline to ordinary Git reads.
    let private_git = repo.store().backend_impl::<GitBackend>().context("internal Git backend disappeared")?.git_repo_path();
    crate::atomic::write_bytes_bounded(&private_git.join("HEAD"), format!("{}\n", request.expected_head.0).as_bytes(), 128)?;
    ensure!(gix::open(private_git)?.head()?.id().context("initial Git HEAD remains unborn")?.detach().to_string() == request.expected_head.0,
        "private Git HEAD differs from the accepted baseline");
    phase(original, &mut check_owner)?;
    let operation = JjOperationId(repo.op_id().hex());
    let stage = store.begin_staging(Duration::from_secs(120))?;
    let id = uuid::Uuid::new_v4().to_string();
    let profile_value = WorkspaceProfile::source_only();
    let policy_value = SourceProjectionPolicy::from_import_policy(&profile_value.source_exclusions())?;
    let policy = staged(&store, &stage.id, ObjectKind::Blob, &policy_value)?;
    let workspace_profile = staged(&store, &stage.id, ObjectKind::Blob, &profile_value)?;
    let hardlinks = staged(&store, &stage.id, ObjectKind::Blob, &HardlinkTopology { format_version: 1, groups: vec![] })?;
    let (record, _) = materialize_source(store.clone(), &stage.id, &commit.tree(), &policy, None, projection_budget(original)?).await?;
    phase(original, &mut check_owner)?;
    let source_root = staged(&store, &stage.id, ObjectKind::SourceRoot, &SourceV2 { format_version: 2,
        consistency: CheckpointConsistency::Quiesced, directory: record.directory.clone(), projection_policy: policy.clone() })?;
    let verified = verify_versioned_source(store.clone(), &stage.id, &source_root, &policy,
        &commit.tree(), Some(&record), projection_budget(original)?).await?;
    phase(original, &mut check_owner)?;
    let runnable_root = staged(&store, &stage.id, ObjectKind::RunnableRoot, &RunnableRoot { format_version: 1,
        consistency: CheckpointConsistency::Quiesced, source: source_root.clone(), environment: Some(request.environment.clone()),
        view_root: record.directory, profile:workspace_profile, hardlinks, knowledge_cursor: 0 })?;
    let runnable_pin = match &mut retention {
        InitialEnvironmentRetention::Local=>{let pin=format!("initial-runnable:{id}");
            store.publish_extending(&stage.id,&pin,&runnable_root,&[request.environment.clone()])?;pin}
        #[cfg(target_os="linux")] InitialEnvironmentRetention::Backed(owner)=>{
            owner.publish_runnable_retaining(&store,&stage.id,&runnable_root,original,&cancelled)?;owner.pin_name().to_owned()}
        #[cfg(not(target_os="linux"))] InitialEnvironmentRetention::_Lifetime(_)=>anyhow::bail!("unsupported environment retention"),
    };
    phase(original, &mut check_owner)?;
    let seed_cache=crate::seed::RetainedSeedCasCache::capture_profile(request.private_repository,store.clone(),profile)?;
    let seed_pin=format!("initial-vcs-seed:{id}");
    let seed_root=match profile {
        crate::seed::SeedProfile::SmallV1=>{
            prepare_internal_metadata_template(request.private_repository,request.seed_destination,baseline.clone(),operation.clone())?;
            phase(original,&mut check_owner)?;
            let seed_stage=store.begin_staging(Duration::from_secs(120))?;
            let root=store.import_tree(request.seed_destination,CheckpointConsistency::Quiesced)?;
            store.publish(&seed_stage.id,&seed_pin,&root)?;root
        }
        crate::seed::SeedProfile::PagedCowV2=>{
            crate::seed::PagedSeedGeneration::prepare_retaining(paged,seed_cache.clone(),baseline.clone(),operation.clone(),seed_pin.clone(),None,
                original.clone(),cancelled,&mut check_owner)?;
            paged.as_ref().unwrap().root()?.clone()
        }
    };
    phase(original,&mut check_owner)?;
    Ok(PreparedInitialSource { _workspace:None,repository: repo, baseline, operation,
        runnable_root, source_root, seed_root, runnable_pin, seed_pin, verified:Arc::new(verified),seed_cache,
        #[cfg(target_os="linux")] runnable_backing:match &retention{InitialEnvironmentRetention::Backed(owner)=>owner.flat_runnable_owner()?,_=>None},
        #[cfg(target_os="linux")] runnable_dependency:match &retention{InitialEnvironmentRetention::Backed(owner)=>owner.dependent_runnable_owner()?,_=>None},
        #[cfg(target_os="linux")] source_parent:match &retention{InitialEnvironmentRetention::Backed(owner)=>owner.source_parent()?,_=>None},
    })
}

#[path="accepted_source_generation.rs"]
mod accepted_generation;
pub use accepted_generation::AcceptedSourceGeneration;

#[cfg(target_os="linux")]
#[path="initial_source_backed.rs"]
pub mod backed;
