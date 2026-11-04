//! Real jj working-copy adapter with injected observation/materialization.
//!
//! No LocalWorkingCopy is used here. The provider owns the scoped RPC, live
//! retained observations, source-projection policy and qualified checkout. A
//! provider must reject checkout when live inode/open-handle semantics are not
//! supported. Production CLI activation waits for that provider qualification.

use crate::{atomic, *};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use jj_lib::commit::Commit;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;
use jj_lib::working_copy::*;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const TYPE_NAME: &str = "rudder-vfs-v1";
const STATE_FILE: &str = "rudder-state.json";

pub type ViewToken = crate::LiveObservation;

pub struct ObservedTree {
    pub tree: MergedTree,
    pub token: ViewToken,
    pub stats: SnapshotStats,
}
pub struct AppliedCheckout {
    pub tree: JjTreeIdentity,
    pub token: ViewToken,
    pub stats: CheckoutStats,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewCapabilities {
    pub checkout: bool,
    pub sparse_checkout: bool,
}

/// Implement outside a restricted guest; the guest talks only to its scoped
/// broker. SnapshotOptions must be honored or explicitly rejected, including
/// tracked/ignored/force-tracked paths, limits and conflict interpretation.
#[async_trait]
pub trait WorkspaceView: Send + Sync {
    fn validate_authority(&self, binding: &AttemptBinding) -> Result<()>;
    /// Qualification is explicit. Unsupported operations fail before recording
    /// an external-start nonce; known rejection is not an uncertain mutation.
    fn capabilities(&self, _binding: &AttemptBinding) -> Result<ViewCapabilities> {
        Ok(ViewCapabilities::default())
    }
    fn bootstrap(&self, binding: &AttemptBinding, store: Arc<Store>) -> Result<ObservedTree>;
    /// Validate the provider's owned retained receipt, not merely a guest's
    /// claimed token. The default is for explicit non-live test providers.
    fn validate_observation(
        &self,
        binding: &AttemptBinding,
        observation: &ViewToken,
        tree: &JjTreeIdentity,
    ) -> Result<()> {
        observation.validate(binding)?;
        tree.validate()
    }
    /// Called only after the working-copy state is durably replaced, under its
    /// private lock. A provider may now adopt the exact nonce and release old
    /// owned pins. On a lost receipt, load retries adoption of persisted state;
    /// it never reruns observation or manufactures a newer filesystem state.
    fn observation_committed(
        &self,
        binding: &AttemptBinding,
        _previous: Option<&ViewToken>,
        current: &ViewToken,
        tree: &JjTreeIdentity,
    ) -> Result<()> {
        self.validate_observation(binding, current, tree)
    }
    async fn observe(
        &self,
        binding: &AttemptBinding,
        previous: Option<&ViewToken>,
        previous_tree: &JjTreeIdentity,
        options: &SnapshotOptions<'_>,
        store: Arc<Store>,
    ) -> Result<ObservedTree>;
    async fn checkout(
        &self,
        binding: &AttemptBinding,
        expected: &ViewToken,
        mutation_id: &str,
        commit: &Commit,
        sparse_patterns: &[RepoPathBuf],
    ) -> Result<AppliedCheckout>;
    async fn set_sparse_patterns(
        &self,
        binding: &AttemptBinding,
        expected: &ViewToken,
        mutation_id: &str,
        tree: &MergedTree,
        patterns: &[RepoPathBuf],
    ) -> Result<AppliedCheckout>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingMutation {
    pub mutation_id: String,
    pub previous_operation: JjOperationId,
    pub previous_tree: JjTreeIdentity,
    pub target_tree: JjTreeIdentity,
    pub target_sparse_patterns: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub(crate) format_version: u32,
    pub(crate) binding: AttemptBinding,
    pub(crate) operation: JjOperationId,
    pub(crate) workspace: String,
    pub(crate) tree: JjTreeIdentity,
    pub(crate) observation: ViewToken,
    pub(crate) cursor_valid: bool,
    pub(crate) sparse_patterns: Vec<String>,
    pub(crate) pending: Option<PendingMutation>,
}
impl State {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported Rudder working-copy state"
        );
        self.binding.validate()?;
        self.operation.validate()?;
        crate::identity::valid_name(&self.workspace)?;
        self.tree.validate()?;
        self.observation.validate(&self.binding)?;
        for path in &self.sparse_patterns {
            RepoPathBuf::from_internal_string(path)?;
        }
        if let Some(pending) = &self.pending {
            crate::identity::valid_name(&pending.mutation_id)?;
            pending.previous_operation.validate()?;
            pending.previous_tree.validate()?;
            pending.target_tree.validate()?;
            for path in &pending.target_sparse_patterns {
                RepoPathBuf::from_internal_string(path)?;
            }
        }
        Ok(())
    }
}

pub struct RudderWorkingCopyFactory {
    view: Arc<dyn WorkspaceView>,
    initialization_binding: Option<AttemptBinding>,
}
impl RudderWorkingCopyFactory {
    pub fn new(
        view: Arc<dyn WorkspaceView>,
        initialization_binding: Option<AttemptBinding>,
    ) -> Self {
        Self {
            view,
            initialization_binding,
        }
    }
}
impl WorkingCopyFactory for RudderWorkingCopyFactory {
    fn init_working_copy(
        &self,
        store: Arc<Store>,
        _working_copy_path: PathBuf,
        state_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        _settings: &UserSettings,
    ) -> std::result::Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let result = (|| {
            let binding = self
                .initialization_binding
                .clone()
                .context("initialization requires an engine-issued attempt binding")?;
            binding.validate()?;
            self.view.validate_authority(&binding)?;
            let observation = self.view.bootstrap(&binding, store.clone())?;
            observation.token.validate(&binding)?;
            ensure!(
                observation.tree.tree_ids().as_resolved() == Some(store.empty_tree_id()),
                "fresh jj initialization requires an empty view; bootstrap the prepared baseline before exposing source"
            );
            let state = State {
                format_version: 1,
                binding,
                operation: JjOperationId(operation_id.hex()),
                workspace: workspace_name.as_str().into(),
                tree: JjTreeIdentity::from_jj(&observation.tree),
                observation: observation.token,
                cursor_valid: true,
                sparse_patterns: vec![String::new()],
                pending: None,
            };
            state.validate()?;
            self.view
                .validate_observation(&state.binding, &state.observation, &state.tree)?;
            publish_state(&state_path, &state)?;
            self.view.observation_committed(
                &state.binding,
                None,
                &state.observation,
                &state.tree,
            )?;
            RudderWorkingCopy::from_state(store, state_path, self.view.clone(), state)
        })();
        result
            .map(|wc| Box::new(wc) as Box<dyn WorkingCopy>)
            .map_err(state_error)
    }
    fn load_working_copy(
        &self,
        store: Arc<Store>,
        _working_copy_path: PathBuf,
        state_path: PathBuf,
        _settings: &UserSettings,
    ) -> std::result::Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let result = (|| {
            let _lock = atomic::lock(&state_path.join("rudder-state.lock"))?;
            let state: State = atomic::read_json(&state_path.join(STATE_FILE))?;
            self.view.validate_authority(&state.binding)?;
            let wc = RudderWorkingCopy::from_state(store, state_path, self.view.clone(), state)?;
            if wc.state.cursor_valid {
                self.view.observation_committed(
                    &wc.state.binding,
                    None,
                    &wc.state.observation,
                    &wc.state.tree,
                )?;
            }
            Ok(wc)
        })();
        result
            .map(|wc| Box::new(wc) as Box<dyn WorkingCopy>)
            .map_err(state_error)
    }
}

pub struct RudderWorkingCopy {
    store: Arc<Store>,
    state_path: PathBuf,
    view: Arc<dyn WorkspaceView>,
    state: State,
    tree: MergedTree,
    operation: OperationId,
    workspace: WorkspaceNameBuf,
    sparse_patterns: Vec<RepoPathBuf>,
}
impl RudderWorkingCopy {
    fn from_state(
        store: Arc<Store>,
        state_path: PathBuf,
        view: Arc<dyn WorkspaceView>,
        state: State,
    ) -> Result<Self> {
        state.validate()?;
        if !view.capabilities(&state.binding)?.sparse_checkout {
            ensure!(
                state.sparse_patterns == [String::new()],
                "this workspace profile supports only the full sparse root"
            );
        }
        Ok(Self {
            tree: state.tree.to_jj(store.clone())?,
            operation: OperationId::new(hex::decode(&state.operation.0)?),
            workspace: state.workspace.clone().into(),
            sparse_patterns: state
                .sparse_patterns
                .iter()
                .map(|s| RepoPathBuf::from_internal_string(s))
                .collect::<std::result::Result<_, _>>()?,
            store,
            state_path,
            view,
            state,
        })
    }
}
impl WorkingCopy for RudderWorkingCopy {
    fn name(&self) -> &str {
        TYPE_NAME
    }
    fn workspace_name(&self) -> &WorkspaceName {
        &self.workspace
    }
    fn operation_id(&self) -> &OperationId {
        &self.operation
    }
    fn tree(&self) -> std::result::Result<&MergedTree, WorkingCopyStateError> {
        Ok(&self.tree)
    }
    fn sparse_patterns(&self) -> std::result::Result<&[RepoPathBuf], WorkingCopyStateError> {
        Ok(&self.sparse_patterns)
    }
    fn start_mutation(
        &self,
    ) -> std::result::Result<Box<dyn LockedWorkingCopy>, WorkingCopyStateError> {
        let result = (|| {
            let lock = atomic::lock(&self.state_path.join("rudder-state.lock"))?;
            let current: State = atomic::read_json(&self.state_path.join(STATE_FILE))?;
            current.validate()?;
            ensure!(
                current == self.state,
                "working-copy state changed since load"
            );
            ensure!(
                current.pending.is_none(),
                "previous filesystem mutation is uncertain; reconcile its exact nonce before using this working copy"
            );
            self.view.validate_authority(&current.binding)?;
            Ok(Box::new(LockedRudderWorkingCopy {
                _lock: lock,
                old_operation: self.operation.clone(),
                old_tree: self.tree.clone(),
                old_observation: current.observation.clone(),
                external_applied: false,
                wc: RudderWorkingCopy::from_state(
                    self.store.clone(),
                    self.state_path.clone(),
                    self.view.clone(),
                    current,
                )?,
            }) as Box<dyn LockedWorkingCopy>)
        })();
        result.map_err(state_error)
    }
}

struct LockedRudderWorkingCopy {
    _lock: File,
    old_operation: OperationId,
    old_tree: MergedTree,
    old_observation: ViewToken,
    wc: RudderWorkingCopy,
    external_applied: bool,
}
impl LockedRudderWorkingCopy {
    fn start_external_mutation(
        &mut self,
        tree: JjTreeIdentity,
        patterns: &[RepoPathBuf],
    ) -> Result<String> {
        ensure!(
            self.wc.state.pending.is_none(),
            "unreconciled external mutation"
        );
        let id = uuid::Uuid::new_v4().to_string();
        self.wc.state.pending = Some(PendingMutation {
            mutation_id: id.clone(),
            previous_operation: self.wc.state.operation.clone(),
            previous_tree: self.wc.state.tree.clone(),
            target_tree: tree,
            target_sparse_patterns: patterns
                .iter()
                .map(|p| p.as_internal_file_string().into())
                .collect(),
        });
        publish_state(&self.wc.state_path, &self.wc.state)?;
        Ok(id)
    }
    fn apply(&mut self, applied: AppliedCheckout) -> Result<CheckoutStats> {
        applied.token.validate(&self.wc.state.binding)?;
        let pending = self
            .wc
            .state
            .pending
            .as_ref()
            .context("mutation nonce missing")?;
        ensure!(
            applied.tree == pending.target_tree,
            "checkout result differs from intended tree"
        );
        ensure!(
            applied.token.view_id == self.wc.state.observation.view_id
                && applied.token.cursor_namespace == self.wc.state.observation.cursor_namespace
                && applied.token.end_generation >= self.wc.state.observation.end_generation,
            "checkout returned another view/namespace or stale generation"
        );
        self.wc
            .view
            .validate_observation(&self.wc.state.binding, &applied.token, &applied.tree)?;
        self.wc.state.tree = applied.tree;
        self.wc.state.observation = applied.token;
        self.wc.state.cursor_valid = true;
        self.wc.tree = self.wc.state.tree.to_jj(self.wc.store.clone())?;
        self.external_applied = true;
        Ok(applied.stats)
    }
}
#[async_trait]
impl LockedWorkingCopy for LockedRudderWorkingCopy {
    fn old_operation_id(&self) -> &OperationId {
        &self.old_operation
    }
    fn old_tree(&self) -> &MergedTree {
        &self.old_tree
    }
    async fn snapshot(
        &mut self,
        options: &SnapshotOptions,
    ) -> std::result::Result<(MergedTree, SnapshotStats), SnapshotError> {
        let result = async {
            ensure!(
                self.wc.state.pending.is_none(),
                "cannot observe across uncertain checkout"
            );
            self.wc.view.validate_authority(&self.wc.state.binding)?;
            let previous = self
                .wc
                .state
                .cursor_valid
                .then_some(&self.wc.state.observation);
            let observed = self
                .wc
                .view
                .observe(
                    &self.wc.state.binding,
                    previous,
                    &self.wc.state.tree,
                    options,
                    self.wc.store.clone(),
                )
                .await?;
            observed.token.validate(&self.wc.state.binding)?;
            self.wc.view.validate_observation(
                &self.wc.state.binding,
                &observed.token,
                &JjTreeIdentity::from_jj(&observed.tree),
            )?;
            ensure!(
                observed.token.view_id == self.wc.state.observation.view_id,
                "observation returned another view"
            );
            if observed.token.cursor_namespace == self.wc.state.observation.cursor_namespace {
                ensure!(
                    observed.token.end_generation >= self.wc.state.observation.end_generation,
                    "observation returned stale generation"
                );
            }
            // Different namespaces have incomparable generations. The provider
            // must return a complete retained tree or reject cursor loss; it
            // must never treat an old dirty hint as an empty/clean delta.
            self.wc.state.tree = JjTreeIdentity::from_jj(&observed.tree);
            self.wc.state.observation = observed.token;
            self.wc.state.cursor_valid = true;
            self.wc.tree = observed.tree.clone();
            Ok((observed.tree, observed.stats))
        }
        .await;
        result.map_err(|err| SnapshotError::WorkingCopyStateError(state_error(err)))
    }
    async fn check_out(
        &mut self,
        commit: &Commit,
    ) -> std::result::Result<CheckoutStats, CheckoutError> {
        let result = async {
            self.wc.view.validate_authority(&self.wc.state.binding)?;
            ensure!(
                self.wc.state.cursor_valid,
                "working-copy metadata was reset; snapshot the actual source before checkout"
            );
            if JjTreeIdentity::from_jj(&commit.tree()) == self.wc.state.tree {
                return Ok(CheckoutStats::default());
            }
            ensure!(
                self.wc.view.capabilities(&self.wc.state.binding)?.checkout,
                "live checkout is unsupported by this workspace profile"
            );
            let patterns = self.wc.sparse_patterns.clone();
            let nonce =
                self.start_external_mutation(JjTreeIdentity::from_jj(&commit.tree()), &patterns)?;
            let applied = self
                .wc
                .view
                .checkout(
                    &self.wc.state.binding,
                    &self.wc.state.observation,
                    &nonce,
                    commit,
                    &patterns,
                )
                .await?;
            self.apply(applied)
        }
        .await;
        result.map_err(|err| CheckoutError::WorkingCopyStateError(state_error(err)))
    }
    fn rename_workspace(&mut self, name: WorkspaceNameBuf) {
        self.wc.workspace = name;
        self.wc.state.workspace = self.wc.workspace.as_str().into();
    }
    async fn reset(&mut self, commit: &Commit) -> std::result::Result<(), ResetError> {
        self.wc
            .view
            .validate_authority(&self.wc.state.binding)
            .map_err(|err| ResetError::WorkingCopyStateError(state_error(err)))?;
        self.wc.tree = commit.tree();
        self.wc.state.tree = JjTreeIdentity::from_jj(&self.wc.tree);
        // Metadata-only reset does not alter source. An old cursor alone no
        // longer proves how the newly tracked tree projects the live bytes.
        self.wc.state.cursor_valid = false;
        Ok(())
    }
    async fn recover(&mut self, commit: &Commit) -> std::result::Result<(), ResetError> {
        self.reset(commit).await
    }
    fn sparse_patterns(&self) -> std::result::Result<&[RepoPathBuf], WorkingCopyStateError> {
        Ok(&self.wc.sparse_patterns)
    }
    async fn set_sparse_patterns(
        &mut self,
        patterns: Vec<RepoPathBuf>,
    ) -> std::result::Result<CheckoutStats, CheckoutError> {
        let result = async {
            self.wc.view.validate_authority(&self.wc.state.binding)?;
            if patterns == self.wc.sparse_patterns {
                return Ok(CheckoutStats::default());
            }
            ensure!(
                self.wc
                    .view
                    .capabilities(&self.wc.state.binding)?
                    .sparse_checkout,
                "sparse checkout is unsupported by this workspace profile"
            );
            let nonce = self.start_external_mutation(self.wc.state.tree.clone(), &patterns)?;
            let applied = self
                .wc
                .view
                .set_sparse_patterns(
                    &self.wc.state.binding,
                    &self.wc.state.observation,
                    &nonce,
                    &self.wc.tree,
                    &patterns,
                )
                .await?;
            let stats = self.apply(applied)?;
            self.wc.state.sparse_patterns = patterns
                .iter()
                .map(|p| p.as_internal_file_string().into())
                .collect();
            self.wc.sparse_patterns = patterns;
            Ok(stats)
        }
        .await;
        result.map_err(|err| CheckoutError::WorkingCopyStateError(state_error(err)))
    }
    async fn finish(
        mut self: Box<Self>,
        operation_id: OperationId,
    ) -> std::result::Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let result = (|| {
            self.wc.view.validate_authority(&self.wc.state.binding)?;
            ensure!(
                self.wc.state.pending.is_none() || self.external_applied,
                "external filesystem mutation is uncertain; cannot finish or replay it"
            );
            self.wc.state.operation = JjOperationId(operation_id.hex());
            self.wc.state.pending = None;
            self.wc.state.validate()?;
            publish_state(&self.wc.state_path, &self.wc.state)?;
            if self.wc.state.cursor_valid {
                self.wc.view.observation_committed(
                    &self.wc.state.binding,
                    Some(&self.old_observation),
                    &self.wc.state.observation,
                    &self.wc.state.tree,
                )?;
            }
            RudderWorkingCopy::from_state(
                self.wc.store.clone(),
                self.wc.state_path.clone(),
                self.wc.view.clone(),
                self.wc.state.clone(),
            )
        })();
        result
            .map(|wc| Box::new(wc) as Box<dyn WorkingCopy>)
            .map_err(state_error)
    }
}

/// The caller may inspect uncertainty, but never discard/replay its nonce.
pub fn pending_mutation(state_path: &Path) -> Result<Option<PendingMutation>> {
    let state: State = atomic::read_json(&state_path.join(STATE_FILE))?;
    state.validate()?;
    Ok(state.pending)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedWorkingCopyReceipt {
    pub format_version: u32,
    pub binding: AttemptBinding,
    pub operation: JjOperationId,
    pub commit: CommitIdentity,
    pub observation: ViewToken,
}

/// Bind an already prepared, possibly nonempty PRIVATE metadata repository.
/// The owning provider must verify that its retained source projection really
/// equals the exact stored WC commit. This never initializes an empty fake
/// observation, scans the source, checks out a tree or changes history.
///
/// The repository is engine-private and not yet exposed to an attempt. Hold
/// these locks through the final type/readiness replacement; ordinary CLI
/// activation must not race a LocalWorkingCopy process in this directory.
pub async fn bind_prepared_working_copy(
    metadata: &Path,
    binding: &AttemptBinding,
    settings: &UserSettings,
    view: Arc<dyn WorkspaceView>,
    observed: ObservedTree,
) -> Result<PreparedWorkingCopyReceipt> {
    use jj_lib::repo::{Repo, RepoLoader, StoreFactories};
    binding.validate()?;
    view.validate_authority(binding)?;
    let _attempt_lock = atomic::lock(&metadata.join("rudder-attempt.lock"))?;
    let state_path = metadata.join(".jj/working_copy");
    ensure!(
        std::fs::symlink_metadata(&state_path)?.is_dir(),
        "private working-copy metadata must be a real directory"
    );
    let _state_lock = atomic::lock(&state_path.join("rudder-state.lock"))?;
    let loader = RepoLoader::init_from_file_system(
        settings,
        &metadata.join(".jj/repo"),
        &StoreFactories::default(),
    )?;
    let _op_lock = loader.op_heads_store().lock().await?;
    let heads = loader.op_heads_store().get_op_heads().await?;
    ensure!(
        heads.len() == 1 && heads[0].hex() == binding.baseline_operation.0,
        "private metadata is not at the exact prepared operation head"
    );
    let operation = loader.load_operation(&heads[0]).await?;
    let repo = loader.load_at(&operation).await?;
    let commit_id = repo
        .view()
        .get_wc_commit_id(WorkspaceName::DEFAULT)
        .context("prepared WC commit missing")?;
    let commit = repo.store().get_commit(commit_id)?;
    ensure!(
        CommitIdentity::from_jj(&commit) == binding.baseline,
        "prepared WC commit/change/tree differs from the issued baseline"
    );
    ensure!(
        crate::projection_io::git_backend(repo.store())?
            .git_repo_path()
            .canonicalize()?
            == crate::projection_io::git_backend(observed.tree.store())?
                .git_repo_path()
                .canonicalize()?,
        "prepared observation belongs to another private jj store"
    );
    let tree = JjTreeIdentity::from_jj(&observed.tree);
    ensure!(
        tree == binding.baseline.tree,
        "retained source projection differs from the stored baseline tree"
    );
    observed.token.validate(binding)?;
    view.validate_observation(binding, &observed.token, &tree)?;
    let type_file = state_path.join("type");
    let mut bytes = String::new();
    use std::io::Read as _;
    atomic::open_read(&type_file)?
        .take(256)
        .read_to_string(&mut bytes)?;
    ensure!(
        matches!(bytes.as_str(), "local" | TYPE_NAME),
        "unsupported prepared working-copy type"
    );
    let state = State {
        format_version: 1,
        binding: binding.clone(),
        operation: binding.baseline_operation.clone(),
        workspace: WorkspaceName::DEFAULT.as_str().into(),
        tree,
        observation: observed.token,
        cursor_valid: true,
        sparse_patterns: vec![String::new()],
        pending: None,
    };
    state.validate()?;
    if state_path.join(STATE_FILE).try_exists()? {
        let previous: State = atomic::read_json(&state_path.join(STATE_FILE))?;
        previous.validate()?;
        ensure!(
            previous == state,
            "existing Rudder binding differs; do not overwrite a live or uncertain attempt"
        );
    }
    publish_state(&state_path, &state)?;
    // A crash before/after this hook retains at least the new pin. A retry
    // adopts the same persisted nonce; it does not resnapshot current source.
    view.observation_committed(binding, None, &state.observation, &state.tree)?;
    atomic::write_bytes_bounded(&type_file, TYPE_NAME.as_bytes(), 256)?;
    let receipt = PreparedWorkingCopyReceipt {
        format_version: 1,
        binding: binding.clone(),
        operation: state.operation,
        commit: CommitIdentity::from_jj(&commit),
        observation: state.observation,
    };
    atomic::write_json(&metadata.join("rudder-baseline-ready.json"), &receipt)?;
    Ok(receipt)
}

/// Recover the SAME persisted nonempty binding after a lost type/readiness or
/// pin-adoption receipt. Never take a fresh observation or silently reset an
/// already advanced attempt. bind_prepared_working_copy rechecks private heads
/// and the exact complete state under both locks before any replacement.
pub async fn reconcile_prepared_working_copy(
    metadata: &Path,
    binding: &AttemptBinding,
    settings: &UserSettings,
    view: Arc<dyn WorkspaceView>,
) -> Result<PreparedWorkingCopyReceipt> {
    use jj_lib::repo::{RepoLoader, StoreFactories};
    let state_path = metadata.join(".jj/working_copy");
    let state: State = {
        let _lock = atomic::lock(&state_path.join("rudder-state.lock"))?;
        let state: State = atomic::read_json(&state_path.join(STATE_FILE))?;
        state.validate()?;
        ensure!(
            state.binding == *binding
                && state.operation == binding.baseline_operation
                && state.tree == binding.baseline.tree
                && state.pending.is_none()
                && state.cursor_valid,
            "persisted state is not the same pending prepared baseline"
        );
        state
    };
    let loader = RepoLoader::init_from_file_system(
        settings,
        &metadata.join(".jj/repo"),
        &StoreFactories::default(),
    )?;
    let observed = ObservedTree {
        tree: state.tree.to_jj(loader.store().clone())?,
        token: state.observation,
        stats: SnapshotStats::default(),
    };
    bind_prepared_working_copy(metadata, binding, settings, view, observed).await
}

pub(crate) fn rebind_private_fork(
    state_path: &Path,
    binding: &AttemptBinding,
    observation: &LiveObservation,
) -> Result<()> {
    let mut state: State = atomic::read_json(&state_path.join(STATE_FILE))?;
    state.validate()?;
    ensure!(
        state.pending.is_none(),
        "cannot fork uncertain working-copy metadata"
    );
    ensure!(
        state.tree == binding.baseline.tree && state.operation == binding.baseline_operation,
        "private metadata is not at the authorized prepared baseline"
    );
    observation.validate(binding)?;
    state.binding = binding.clone();
    // The engine must issue a new view-specific observation before activation;
    // do not transplant another attempt's retained live cursor/pin as current.
    state.cursor_valid = false;
    state.observation = observation.clone();
    publish_state(state_path, &state)
}

/// Every trusted WC-state publisher holds this distinct lock only during the
/// durable replacement. The RPC witness can take it while the caller still
/// owns rudder-state.lock, without recursively acquiring that WC lock.
fn publish_state(state_path: &Path, state: &State) -> Result<()> {
    let _publication = atomic::lock(&state_path.join("rudder-publication.lock"))?;
    atomic::write_json(&state_path.join(STATE_FILE), state)
}

fn state_error(err: anyhow::Error) -> WorkingCopyStateError {
    WorkingCopyStateError {
        message: format!("Rudder working copy: {err:#}"),
        err: err.into(),
    }
}
