//! Host-side consumer of the scoped owning-view RPC. CAS and jj metadata stay
//! outside the guest. There is no mounted-view DB reopen or source walk.

use crate::atomic;
use crate::projection::{self, ProjectionRecord};
use crate::projection_io::{ProjectionBudget, ProjectionWork};
use crate::vfs_provider::{
    self, AttemptAuthority, MAX_PENDING_RECORDS, MAX_RECORD_BYTES, OwnedProjection,
    SourceProjectionPolicy,
};
use crate::working_copy::*;
use crate::{AttemptBinding, JjTreeIdentity, LiveObservation};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use jj_lib::commit::Commit;
use jj_lib::merged_tree::MergedTree;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::{Repo, RepoLoader, StoreFactories};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;
use jj_lib::working_copy::SnapshotOptions;
use rudder_engine::protocol::canonical_json;
use rudder_engine::store::LocalStore;
use rudder_engine::vfs::rpc::{
    AdoptionEvidence, AdoptionGuard, AdoptionWitness, ObservationBinding, ObservationClient,
    ObservationReceipt, Operation, ReceiptState, Reply,
};
use rudder_engine::vfs::{VfsObservation, exclusion_policy_digest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Protected host configuration. Never project this file/CAS path into a guest.
/// A guest receives only the separately scoped semantic-command endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcViewConfig {
    pub format_version: u32,
    pub attempt: AttemptBinding,
    pub observation: ObservationBinding,
    pub socket: PathBuf,
    pub identity_file: PathBuf,
    pub private_repository: PathBuf,
    pub private_git: PathBuf,
    pub projection_state: PathBuf,
    pub cas_root: PathBuf,
    pub policy: SourceProjectionPolicy,
    pub budget: ProjectionBudget,
}
impl RpcViewConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported RPC view configuration"
        );
        self.attempt.validate()?;
        self.observation.validate()?;
        ensure!(
            self.attempt.view_id == self.observation.view_id
                && self.attempt.owner == self.observation.owner
                && self.attempt.profile == self.observation.profile,
            "workspace RPC and VCS attempt bindings differ"
        );
        ensure!(
            self.policy.reference()? == self.attempt.projection_policy
                && exclusion_policy_digest(&self.policy.import_policy()?)?
                    == self.observation.exclusion_policy_digest,
            "VCS policy differs from admitted workspace source complement"
        );
        ensure!(
            canonical_json(&self.attempt.baseline.tree)?.len() <= 8192,
            "baseline tree exceeds qualified RPC evidence bound"
        );
        for path in [
            &self.socket,
            &self.identity_file,
            &self.private_repository,
            &self.private_git,
            &self.projection_state,
            &self.cas_root,
        ] {
            ensure!(path.is_absolute(), "host workspace paths must be absolute");
        }
        ensure!(
            self.budget.max_elapsed_ms > 0
                && self.budget.max_elapsed_ms <= 30_000
                && self.budget.max_entries <= 100_000
                && self.budget.max_metadata_bytes <= 128 * 1024 * 1024
                && self.budget.max_content_bytes <= 256 * 1024 * 1024
                && self.budget.max_jj_buffer_bytes <= 32 * 1024 * 1024
                && self.budget.max_depth <= 256,
            "projection budget exceeds qualified RPC profile"
        );
        Ok(())
    }
    pub fn working_copy_state(&self) -> PathBuf {
        self.private_repository.join(".jj/working_copy")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingCapture {
    format_version: u32,
    binding: AttemptBinding,
    nonce: String,
    baseline: JjTreeIdentity,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingAdoption {
    format_version: u32,
    binding: AttemptBinding,
    operation_id: String,
    expected: Option<String>,
    nonce: String,
    evidence: AdoptionEvidence,
}

pub struct RpcWorkspaceView {
    config: RpcViewConfig,
    store: LocalStore,
    authority: Arc<dyn AttemptAuthority>,
    client: ObservationClient,
}
impl RpcWorkspaceView {
    pub fn new(
        config: RpcViewConfig,
        store: LocalStore,
        authority: Arc<dyn AttemptAuthority>,
    ) -> Result<Self> {
        config.validate()?;
        ensure!(
            store.root().canonicalize()? == config.cas_root.canonicalize()?,
            "RPC provider was given another private CAS"
        );
        ensure!(
            store.read_object(&config.attempt.projection_policy)?
                == canonical_json(&config.policy)?,
            "stored source policy differs"
        );
        let profile: rudder_engine::workspace::WorkspaceProfile =
            serde_json::from_slice(&store.read_object(&config.attempt.profile)?)?;
        profile.validate()?;
        ensure!(
            exclusion_policy_digest(&profile.source_exclusions())?
                == config.observation.exclusion_policy_digest,
            "profile does not authorize these source exclusions"
        );
        vfs_provider::private_directory(&config.projection_state)?;
        vfs_provider::private_directory(&config.projection_state.join("observations"))?;
        let client = ObservationClient::new(
            &config.socket,
            &config.identity_file,
            config.observation.clone(),
        )?;
        let view = Self {
            config,
            store,
            authority,
            client,
        };
        view.validate_authority(&view.config.attempt)?;
        Ok(view)
    }
    pub fn config(&self) -> &RpcViewConfig {
        &self.config
    }
    pub fn prepare_baseline(
        &self,
        baseline: &MergedTree,
        options: Option<&SnapshotOptions<'_>>,
    ) -> Result<ObservedTree> {
        ensure!(
            JjTreeIdentity::from_jj(baseline) == self.config.attempt.baseline.tree,
            "baseline differs from issued attempt"
        );
        pollster::block_on(self.capture(baseline, None, options))
    }
    pub fn projection_for(&self, token: &LiveObservation) -> Result<ProjectionRecord> {
        Ok(self.read_owned(token)?.projection)
    }
    pub fn work_for(&self, token: &LiveObservation) -> Result<ProjectionWork> {
        Ok(self.read_owned(token)?.work)
    }
    fn record_path(&self, nonce: &str) -> Result<PathBuf> {
        let nonce = uuid::Uuid::parse_str(nonce)?;
        Ok(self
            .config
            .projection_state
            .join("observations")
            .join(format!("{nonce}.json")))
    }
    fn capture_path(&self) -> PathBuf {
        self.config.projection_state.join("capture-pending.json")
    }
    fn adoption_path(&self) -> PathBuf {
        self.config.projection_state.join("adoption-pending.json")
    }
    fn inspect(&self, nonce: &str) -> Result<ObservationReceipt> {
        match self.client.call(Operation::Inspect {
            nonce: nonce.into(),
        })? {
            Reply::Observation(receipt) => Ok(receipt),
            _ => bail!("workspace RPC returned the wrong observation result"),
        }
    }
    fn read_owned(&self, token: &LiveObservation) -> Result<OwnedProjection> {
        token.validate(&self.config.attempt)?;
        let owned: OwnedProjection =
            atomic::read_json_bounded(&self.record_path(&token.request_nonce)?, MAX_RECORD_BYTES)?;
        owned.projection.validate()?;
        ensure!(
            owned.format_version == 1
                && owned.binding == self.config.attempt
                && owned.observation == *token
                && owned.projection.directory == token.directory
                && owned.projection.projection_policy == self.config.attempt.projection_policy,
            "projection is not this RPC provider's exact owned receipt"
        );
        ensure!(
            self.store.retention()?.pins.get(&token.retention) == Some(&token.directory),
            "source observation pin is unavailable"
        );
        Ok(owned)
    }
    fn verify_receipt(&self, receipt: &ObservationReceipt) -> Result<VfsObservation> {
        ensure!(
            receipt.retained
                && matches!(
                    receipt.state,
                    ReceiptState::Retained | ReceiptState::Adopted
                ),
            "workspace observation is pending or retired"
        );
        let observation = receipt
            .observation
            .clone()
            .context("workspace observation has no confirmed retained result")?;
        ensure!(
            observation.request_nonce == receipt.nonce
                && observation.view_id == self.config.observation.view_id
                && observation.owner == self.config.observation.owner
                && observation.begin.namespace == self.config.observation.namespace
                && observation.end.namespace == self.config.observation.namespace
                && observation.exclusion_policy_digest
                    == self.config.observation.exclusion_policy_digest,
            "workspace observation scope changed"
        );
        LiveObservation::from_vfs(&observation, &self.config.attempt)?;
        Ok(observation)
    }
    /// Inspect the ORIGINAL uncertain capture. It remains the old immutable
    /// result after later writes/restart, never a fresh observation in disguise.
    pub fn inspect_pending_capture(&self) -> Result<Option<ObservationReceipt>> {
        self.validate_authority(&self.config.attempt)?;
        if !self.capture_path().try_exists()? {
            return Ok(None);
        }
        let pending: PendingCapture = atomic::read_json(&self.capture_path())?;
        ensure!(
            pending.format_version == 1 && pending.binding == self.config.attempt,
            "pending capture belongs to another attempt"
        );
        Ok(Some(self.inspect(&pending.nonce)?))
    }
    /// Retire only an uncommitted candidate, under the private command and
    /// publication locks. A live command or a WC reference prevents cleanup.
    pub fn retire_uncommitted_capture(&self) -> Result<()> {
        use fs2::FileExt;
        let mut options = std::fs::OpenOptions::new();
        use std::os::unix::fs::OpenOptionsExt;
        let command = options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.config.private_repository.join("rudder-command.lock"))?;
        command
            .try_lock_exclusive()
            .context("a private VCS command is still live")?;
        let _projection = atomic::lock(&self.config.projection_state.join("projection.lock"))?;
        let _publication = atomic::lock(
            &self
                .config
                .working_copy_state()
                .join("rudder-publication.lock"),
        )?;
        let pending: PendingCapture = atomic::read_json(&self.capture_path())?;
        ensure!(
            pending.binding == self.config.attempt && pending.format_version == 1,
            "pending capture binding differs"
        );
        let state_path = self.config.working_copy_state().join("rudder-state.json");
        if state_path.try_exists()? {
            let state: crate::working_copy::State = atomic::read_json(&state_path)?;
            state.validate()?;
            ensure!(
                state.binding == self.config.attempt
                    && state.observation.request_nonce != pending.nonce,
                "persisted WC still references this observation"
            );
        }
        let inspected = self.inspect(&pending.nonce)?;
        ensure!(
            !inspected.current && inspected.state != ReceiptState::Adopted,
            "cannot retire an adopted observation"
        );
        match self.client.call(Operation::Release {
            nonce: pending.nonce,
        })? {
            Reply::Observation(result) => ensure!(
                result.state == ReceiptState::Released && !result.retained,
                "observation release is not confirmed"
            ),
            _ => bail!("wrong retirement result"),
        }
        remove_durable(&self.capture_path())
    }
    fn check_store(&self, store: &Store) -> Result<()> {
        ensure!(
            crate::projection_io::git_backend(store)?
                .git_repo_path()
                .canonicalize()?
                == self.config.private_git.canonicalize()?,
            "RPC provider cannot access another private jj store"
        );
        Ok(())
    }
    async fn capture(
        &self,
        baseline: &MergedTree,
        previous: Option<&LiveObservation>,
        options: Option<&SnapshotOptions<'_>>,
    ) -> Result<ObservedTree> {
        self.validate_authority(&self.config.attempt)?;
        self.check_store(baseline.store())?;
        let _projection = atomic::lock(&self.config.projection_state.join("projection.lock"))?;
        ensure!(
            !self.capture_path().try_exists()?,
            "a workspace observation is uncertain; inspect/retire its exact nonce before taking another snapshot"
        );
        ensure!(
            fs::read_dir(self.config.projection_state.join("observations"))?
                .take(MAX_PENDING_RECORDS + 1)
                .count()
                < MAX_PENDING_RECORDS,
            "projection receipt budget exhausted"
        );
        for excluded in self.config.policy.import_policy()?.excluded_paths {
            if let Ok(names) = excluded
                .iter()
                .map(|n| std::str::from_utf8(n))
                .collect::<std::result::Result<Vec<_>, _>>()
            {
                let path = RepoPathBuf::from_internal_string(names.join("/"))?;
                ensure!(
                    !baseline.path_value(&path).await?.is_present(),
                    "workspace profile excludes already tracked source"
                );
            }
        }
        let prior = previous.map(|p| self.read_owned(p)).transpose()?;
        let nonce = uuid::Uuid::new_v4().to_string();
        atomic::write_json(
            &self.capture_path(),
            &PendingCapture {
                format_version: 1,
                binding: self.config.attempt.clone(),
                nonce: nonce.clone(),
                baseline: JjTreeIdentity::from_jj(baseline),
            },
        )?;
        let receipt = match self.client.call(Operation::Observe {
            nonce: nonce.clone(),
        })? {
            Reply::Observation(value) => value,
            _ => bail!("wrong capture result"),
        };
        let observed = self.verify_receipt(&receipt)?;
        ensure!(
            observed.request_nonce == nonce,
            "capture returned another nonce"
        );
        let token = LiveObservation::from_vfs(&observed, &self.config.attempt)?;
        let projected = projection::project_source(
            self.store.clone(),
            &observed.directory,
            baseline,
            &self.config.attempt.projection_policy,
            options,
            prior.as_ref().map(|p| &p.projection),
            self.config.budget.clone(),
        )
        .await?;
        ensure!(
            canonical_json(&JjTreeIdentity::from_jj(&projected.tree))?.len() <= 8192,
            "projected tree exceeds qualified RPC evidence bound"
        );
        self.validate_authority(&self.config.attempt)?;
        let owned = OwnedProjection {
            format_version: 1,
            binding: self.config.attempt.clone(),
            observation: token.clone(),
            projection: projected.record,
            work: projected.work,
        };
        let path = self.record_path(&nonce)?;
        ensure!(
            !path.try_exists()?,
            "observation nonce already has a projection receipt"
        );
        atomic::write_json_bounded(&path, &owned, MAX_RECORD_BYTES)?;
        remove_durable(&self.capture_path())?;
        Ok(ObservedTree {
            tree: projected.tree,
            token,
            stats: projected.stats,
        })
    }
    fn adopt(
        &self,
        previous: Option<&LiveObservation>,
        current: &LiveObservation,
        tree: &JjTreeIdentity,
    ) -> Result<()> {
        self.validate_observation(&self.config.attempt, current, tree)?;
        let _projection = atomic::lock(&self.config.projection_state.join("projection.lock"))?;
        let evidence = {
            let _publication = atomic::lock(
                &self
                    .config
                    .working_copy_state()
                    .join("rudder-publication.lock"),
            )?;
            let (state, bytes) = read_state(&self.config.working_copy_state())?;
            ensure!(
                state.binding == self.config.attempt
                    && state.observation == *current
                    && state.tree == *tree
                    && state.cursor_valid
                    && state.pending.is_none(),
                "durable WC state does not publish this exact observation/tree"
            );
            AdoptionEvidence {
                state_sha256: hex::encode(Sha256::digest(bytes)),
                tree: serde_json::to_value(tree)?,
            }
        };
        let pending = if self.adoption_path().try_exists()? {
            let pending: PendingAdoption = atomic::read_json(&self.adoption_path())?;
            ensure!(
                pending.format_version == 1
                    && pending.binding == self.config.attempt
                    && pending.nonce == current.request_nonce
                    && pending.evidence == evidence,
                "unreconciled adoption belongs to another exact WC state"
            );
            pending
        } else {
            let head = match self.client.call(Operation::Head)? {
                Reply::Head(head) => head,
                _ => bail!("wrong workspace head result"),
            };
            if let Some(head) = &head {
                if head.current == current.request_nonce {
                    ensure!(
                        head.tree_sha256 == hex::encode(Sha256::digest(canonical_json(tree)?)),
                        "adopted source tree differs"
                    );
                    return Ok(());
                }
                if let Some(previous) = previous {
                    ensure!(
                        head.current == previous.request_nonce,
                        "workspace adopted head no longer matches previous WC state"
                    );
                }
            }
            let pending = PendingAdoption {
                format_version: 1,
                binding: self.config.attempt.clone(),
                operation_id: uuid::Uuid::new_v4().to_string(),
                expected: head.map(|h| h.current),
                nonce: current.request_nonce.clone(),
                evidence,
            };
            atomic::write_json(&self.adoption_path(), &pending)?;
            pending
        };
        let reply = self.client.call(Operation::Adopt {
            operation_id: pending.operation_id.clone(),
            expected_current: pending.expected.clone(),
            nonce: pending.nonce.clone(),
            evidence: pending.evidence.clone(),
        })?;
        let Reply::Adoption(receipt) = reply else {
            bail!("wrong workspace adoption result")
        };
        ensure!(
            receipt.operation_id == pending.operation_id
                && receipt.current == pending.nonce
                && receipt.previous == pending.expected
                && receipt.state_sha256 == pending.evidence.state_sha256
                && receipt.tree_sha256
                    == hex::encode(Sha256::digest(canonical_json(&pending.evidence.tree)?)),
            "adoption receipt differs from exact durable intent"
        );
        remove_durable(&self.adoption_path())?;
        if let Some(old) = &receipt.previous {
            if old != &receipt.current {
                let path = self.record_path(old)?;
                if path.try_exists()? {
                    remove_durable(&path)?;
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl WorkspaceView for RpcWorkspaceView {
    fn validate_authority(&self, binding: &AttemptBinding) -> Result<()> {
        ensure!(
            *binding == self.config.attempt,
            "RPC view belongs to another attempt"
        );
        self.authority.validate(binding)?;
        let Reply::Cursor(cursor) = self.client.call(Operation::Cursor)? else {
            bail!("wrong workspace authority response")
        };
        ensure!(
            cursor.view_id == self.config.observation.view_id
                && cursor.owner == self.config.observation.owner
                && cursor.namespace == self.config.observation.namespace,
            "workspace RPC epoch or namespace changed"
        );
        Ok(())
    }
    fn bootstrap(&self, binding: &AttemptBinding, store: Arc<Store>) -> Result<ObservedTree> {
        self.validate_authority(binding)?;
        let tree = MergedTree::resolved(store.clone(), store.empty_tree_id().clone());
        pollster::block_on(self.capture(&tree, None, None))
    }
    fn validate_observation(
        &self,
        binding: &AttemptBinding,
        observation: &ViewToken,
        tree: &JjTreeIdentity,
    ) -> Result<()> {
        self.validate_authority(binding)?;
        ensure!(
            self.read_owned(observation)?.projection.tree == *tree,
            "owned RPC projection does not prove this jj tree"
        );
        let confirmed = self.verify_receipt(&self.inspect(&observation.request_nonce)?)?;
        ensure!(
            LiveObservation::from_vfs(&confirmed, binding)? == *observation,
            "RPC retained observation differs from WC receipt"
        );
        Ok(())
    }
    fn observation_committed(
        &self,
        binding: &AttemptBinding,
        previous: Option<&ViewToken>,
        current: &ViewToken,
        tree: &JjTreeIdentity,
    ) -> Result<()> {
        ensure!(*binding == self.config.attempt, "attempt changed");
        self.adopt(previous, current, tree)
    }
    async fn observe(
        &self,
        binding: &AttemptBinding,
        previous: Option<&ViewToken>,
        tree: &JjTreeIdentity,
        options: &SnapshotOptions<'_>,
        store: Arc<Store>,
    ) -> Result<ObservedTree> {
        self.validate_authority(binding)?;
        self.capture(&tree.to_jj(store)?, previous, Some(options))
            .await
    }
    async fn checkout(
        &self,
        _: &AttemptBinding,
        _: &ViewToken,
        _: &str,
        _: &Commit,
        _: &[RepoPathBuf],
    ) -> Result<AppliedCheckout> {
        bail!("mounted checkout remains unsupported")
    }
    async fn set_sparse_patterns(
        &self,
        _: &AttemptBinding,
        _: &ViewToken,
        _: &str,
        _: &MergedTree,
        _: &[RepoPathBuf],
    ) -> Result<AppliedCheckout> {
        bail!("mounted sparse checkout remains unsupported")
    }
}

/// Fixed host paths only. The helper constructs this from protected launch
/// configuration; no request can choose a path, owner, source root or git store.
pub struct DurableWorkingCopyWitness {
    config: RpcViewConfig,
    settings: UserSettings,
}
impl DurableWorkingCopyWitness {
    pub fn new(config: RpcViewConfig, settings: UserSettings) -> Result<Self> {
        config.validate()?;
        Ok(Self { config, settings })
    }
}
impl AdoptionWitness for DurableWorkingCopyWitness {
    fn verify<'a>(
        &'a self,
        binding: &ObservationBinding,
        observation: &VfsObservation,
        evidence: &AdoptionEvidence,
    ) -> Result<Box<dyn AdoptionGuard + 'a>> {
        ensure!(
            *binding == self.config.observation,
            "witness belongs to another exact workspace"
        );
        let publication = atomic::lock(
            &self
                .config
                .working_copy_state()
                .join("rudder-publication.lock"),
        )?;
        let (state, bytes) = read_state(&self.config.working_copy_state())?;
        ensure!(
            state.binding == self.config.attempt && state.cursor_valid && state.pending.is_none(),
            "private WC state is not a committed source observation"
        );
        let token = LiveObservation::from_vfs(observation, &self.config.attempt)?;
        ensure!(
            state.observation == token
                && serde_json::to_value(&state.tree)? == evidence.tree
                && hex::encode(Sha256::digest(&bytes)) == evidence.state_sha256,
            "private WC source/tree/nonce/hash differs"
        );
        let nonce = uuid::Uuid::parse_str(&observation.request_nonce)?;
        let owned: OwnedProjection = atomic::read_json_bounded(
            &self
                .config
                .projection_state
                .join("observations")
                .join(format!("{nonce}.json")),
            MAX_RECORD_BYTES,
        )?;
        owned.projection.validate()?;
        ensure!(
            owned.format_version == 1
                && owned.binding == self.config.attempt
                && owned.observation == token
                && owned.projection.directory == observation.directory
                && owned.projection.tree == state.tree
                && owned.projection.projection_policy == self.config.attempt.projection_policy,
            "host projection does not prove published WC state"
        );
        pollster::block_on(async {
            let loader = RepoLoader::init_from_file_system(
                &self.settings,
                &self.config.private_repository.join(".jj/repo"),
                &StoreFactories::default(),
            )?;
            ensure!(
                crate::projection_io::git_backend(loader.store())?
                    .git_repo_path()
                    .canonicalize()?
                    == self.config.private_git.canonicalize()?,
                "witness git store changed"
            );
            let op = loader
                .load_operation(&jj_lib::op_store::OperationId::new(hex::decode(
                    &state.operation.0,
                )?))
                .await?;
            let repo = loader.load_at(&op).await?;
            let wc = repo
                .view()
                .get_wc_commit_id(WorkspaceName::new(&state.workspace))
                .context("published WC operation has no workspace commit")?;
            ensure!(
                JjTreeIdentity::from_jj(&repo.store().get_commit(wc)?.tree()) == state.tree,
                "published private jj operation does not contain this WC tree"
            );
            Result::<()>::Ok(())
        })?;
        atomic::open_read(&self.config.working_copy_state().join("rudder-state.json"))?
            .sync_all()?;
        File::open(self.config.working_copy_state())?.sync_all()?;
        Ok(Box::new(publication))
    }
}

fn read_state(directory: &Path) -> Result<(crate::working_copy::State, Vec<u8>)> {
    let file = atomic::open_read(&directory.join("rudder-state.json"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o600
            && metadata.nlink() == 1
            && metadata.len() <= atomic::MAX_STATE_BYTES,
        "unsafe private WC state file"
    );
    let mut bytes = Vec::new();
    file.take(atomic::MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= atomic::MAX_STATE_BYTES,
        "WC state exceeds bound"
    );
    let state: crate::working_copy::State = serde_json::from_slice(&bytes)?;
    state.validate()?;
    ensure!(
        canonical_json(&state)? == bytes,
        "private WC state is not canonical"
    );
    Ok((state, bytes))
}
fn remove_durable(path: &Path) -> Result<()> {
    fs::remove_file(path)?;
    File::open(path.parent().context("missing publication parent")?)?.sync_all()?;
    Ok(())
}
