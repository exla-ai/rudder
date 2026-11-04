//! Owning-VfsView snapshot provider. No source filesystem walk, mounted DB
//! reopen, LocalWorkingCopy delegation or live checkout fallback exists here.
//! Run this in an owned background helper or scoped broker, not the App actor.

use crate::atomic;
use crate::projection::{self, ProjectionRecord};
use crate::projection_io::{ProjectionBudget, ProjectionWork};
use crate::working_copy::*;
use crate::{AttemptBinding, JjTreeIdentity, LiveObservation};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use jj_lib::commit::Commit;
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::store::Store;
use jj_lib::working_copy::SnapshotOptions;
use rudder_engine::protocol::{ObjectKind, ObjectRef, canonical_json};
use rudder_engine::store::{ImportPolicy, LocalStore};
use rudder_engine::vfs::{ObservationBudget, VfsView};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_PENDING_RECORDS: usize = 256;

/// Version 1 fixes UTF-8 jj paths/targets, executable-bit mode projection and
/// jj Diff conflict markers. The hash identifies this policy PROGRAM and the
/// explicit source exclusions. It does not pretend opaque SnapshotOptions are
/// serializable or that the same program means the same dynamic configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceProjectionPolicy {
    pub format_version: u32,
    pub excluded_paths_hex: Vec<Vec<String>>,
}
impl SourceProjectionPolicy {
    pub fn from_import_policy(policy: &ImportPolicy) -> Result<Self> {
        let mut excluded_paths_hex = Vec::new();
        ensure!(
            policy.excluded_paths.len() <= 4096,
            "too many source exclusions"
        );
        for path in &policy.excluded_paths {
            ensure!(
                !path.is_empty() && path.len() <= 256,
                "invalid source exclusion depth"
            );
            let mut encoded = Vec::new();
            for name in path {
                ensure!(
                    !name.is_empty()
                        && name.len() <= 255
                        && !matches!(name.as_slice(), b"." | b"..")
                        && !name.contains(&0)
                        && !name.contains(&b'/'),
                    "invalid source exclusion component"
                );
                encoded.push(hex::encode(name));
            }
            excluded_paths_hex.push(encoded);
        }
        excluded_paths_hex.sort();
        excluded_paths_hex.dedup();
        Ok(Self {
            format_version: 1,
            excluded_paths_hex,
        })
    }
    pub fn reference(&self) -> Result<ObjectRef> {
        self.import_policy()?;
        Ok(ObjectRef::from_bytes(
            ObjectKind::Blob,
            &canonical_json(self)?,
        ))
    }
    pub fn import_policy(&self) -> Result<ImportPolicy> {
        ensure!(
            self.format_version == 1,
            "unsupported source projection policy"
        );
        let policy = ImportPolicy {
            excluded_paths: self
                .excluded_paths_hex
                .iter()
                .map(|p| {
                    p.iter()
                        .map(|c| hex::decode(c).map_err(anyhow::Error::from))
                        .collect()
                })
                .collect::<Result<_>>()?,
        };
        // Canonical sorting and raw-byte encoding are part of the identity.
        let canonical = Self::from_import_policy(&policy)?;
        ensure!(
            canonical == *self,
            "source projection policy must be canonical"
        );
        Ok(policy)
    }
}

/// The runtime supplies a trusted journal/lease check for current owner, run
/// epoch, exact attempt/view and the actual admitted profile. A JSON binding
/// passed by the guest cannot authorize itself or transfer this provider.
pub trait AttemptAuthority: Send + Sync {
    fn validate(&self, binding: &AttemptBinding) -> Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnedProjection {
    pub(crate) format_version: u32,
    pub(crate) binding: AttemptBinding,
    pub(crate) observation: LiveObservation,
    pub(crate) projection: ProjectionRecord,
    pub(crate) work: ProjectionWork,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Adopted {
    format_version: u32,
    binding: AttemptBinding,
    current: Option<LiveObservation>,
    /// Write this BEFORE release, so lost release receipts are repeatable and
    /// no successful state transition can lose the new retained root.
    release_pending: Vec<String>,
}

pub struct VfsWorkspaceView {
    view: VfsView,
    store: LocalStore,
    binding: AttemptBinding,
    authority: Arc<dyn AttemptAuthority>,
    private_state: PathBuf,
    expected_git_path: PathBuf,
    exclusions: ImportPolicy,
    observation_budget: ObservationBudget,
    projection_budget: ProjectionBudget,
}
impl VfsWorkspaceView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        view: VfsView,
        binding: AttemptBinding,
        authority: Arc<dyn AttemptAuthority>,
        private_state: &Path,
        expected_git_path: &Path,
        policy: SourceProjectionPolicy,
        observation_budget: ObservationBudget,
        projection_budget: ProjectionBudget,
    ) -> Result<Self> {
        binding.validate()?;
        authority.validate(&binding)?;
        let store = view.manager().store().clone();
        ensure!(
            policy.reference()? == binding.projection_policy,
            "binding names another source policy"
        );
        ensure!(
            store.read_object(&binding.projection_policy)? == canonical_json(&policy)?,
            "stored policy bytes differ"
        );
        private_directory(private_state)?;
        private_directory(&private_state.join("observations"))?;
        let provider = Self {
            view,
            store,
            binding,
            authority,
            private_state: private_state.to_path_buf(),
            expected_git_path: expected_git_path.canonicalize()?,
            exclusions: policy.import_policy()?,
            observation_budget,
            projection_budget,
        };
        provider.validate_authority(&provider.binding)?;
        Ok(provider)
    }

    pub fn prepare_baseline(
        &self,
        baseline: &MergedTree,
        options: Option<&SnapshotOptions<'_>>,
    ) -> Result<ObservedTree> {
        ensure!(
            JjTreeIdentity::from_jj(baseline) == self.binding.baseline.tree,
            "baseline tree differs from issued binding"
        );
        pollster::block_on(self.capture(baseline, None, options))
    }

    pub fn work_for(&self, observation: &LiveObservation) -> Result<ProjectionWork> {
        Ok(self.read_owned(observation)?.work)
    }

    pub fn projection_for(&self, observation: &LiveObservation) -> Result<ProjectionRecord> {
        Ok(self.read_owned(observation)?.projection)
    }

    fn record_path(&self, nonce: &str) -> Result<PathBuf> {
        let nonce = uuid::Uuid::parse_str(nonce).context("observation nonce must be a UUID")?;
        Ok(self
            .private_state
            .join("observations")
            .join(format!("{nonce}.json")))
    }

    fn read_owned(&self, observation: &LiveObservation) -> Result<OwnedProjection> {
        observation.validate(&self.binding)?;
        let record: OwnedProjection = atomic::read_json_bounded(
            &self.record_path(&observation.request_nonce)?,
            MAX_RECORD_BYTES,
        )?;
        ensure!(
            record.format_version == 1
                && record.binding == self.binding
                && record.observation == *observation,
            "observation is not this provider's exact owned receipt"
        );
        record.projection.validate()?;
        ensure!(
            record.projection.directory == observation.directory
                && record.projection.projection_policy == self.binding.projection_policy,
            "projection receipt source/policy differs"
        );
        ensure!(
            self.store.retention()?.pins.get(&observation.retention)
                == Some(&observation.directory),
            "observation no longer owns its retained source root"
        );
        Ok(record)
    }

    fn check_jj_store(&self, store: &Store) -> Result<()> {
        let backend = crate::projection_io::git_backend(store)?;
        ensure!(
            backend.git_repo_path().canonicalize()? == self.expected_git_path,
            "provider cannot write another private jj store"
        );
        Ok(())
    }

    async fn capture(
        &self,
        baseline: &MergedTree,
        previous: Option<&LiveObservation>,
        options: Option<&SnapshotOptions<'_>>,
    ) -> Result<ObservedTree> {
        self.validate_authority(&self.binding)?;
        self.check_jj_store(baseline.store())?;
        let _lock = atomic::lock(&self.private_state.join("projection.lock"))?;
        // An environment/scratch exclusion may never silently delete already
        // tracked source. Point lookups avoid another full jj tree traversal.
        for excluded in &self.exclusions.excluded_paths {
            let components = excluded
                .iter()
                .map(|name| std::str::from_utf8(name))
                .collect::<std::result::Result<Vec<_>, _>>();
            if let Ok(components) = components {
                let path = RepoPathBuf::from_internal_string(components.join("/"))?;
                ensure!(
                    !baseline.path_value(&path).await?.is_present(),
                    "workspace profile excludes already tracked source at {path:?}"
                );
            }
        }
        let pending = fs::read_dir(self.private_state.join("observations"))?
            .take(MAX_PENDING_RECORDS + 1)
            .count();
        ensure!(
            pending < MAX_PENDING_RECORDS,
            "too many retained projection receipts; reconcile abandoned observations before continuing"
        );
        let prior = previous.map(|p| self.read_owned(p)).transpose()?;
        let nonce = uuid::Uuid::new_v4().to_string();
        let observed =
            self.view
                .observe_source(&self.exclusions, &nonce, &self.observation_budget)?;
        let token = LiveObservation::from_vfs(&observed, &self.binding)?;
        let projected = match projection::project_source(
            self.store.clone(),
            &observed.directory,
            baseline,
            &self.binding.projection_policy,
            options,
            prior.as_ref().map(|p| &p.projection),
            self.projection_budget.clone(),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                // No WC receipt was published and no namespace was mutated.
                // Retire only this failed capture's owned nonce, never a seed.
                let _ = self.view.release_observation(&nonce);
                return Err(error);
            }
        };
        self.validate_authority(&self.binding)?;
        let record = OwnedProjection {
            format_version: 1,
            binding: self.binding.clone(),
            observation: token.clone(),
            projection: projected.record,
            work: projected.work,
        };
        // Once returned, this pin survives interruption until an exact durable
        // WC state adopts it. Unreturned/crash observations are conservative
        // retained evidence; there is no time-based release or guessed replay.
        atomic::write_json_bounded(&self.record_path(&nonce)?, &record, MAX_RECORD_BYTES)?;
        Ok(ObservedTree {
            tree: projected.tree,
            token,
            stats: projected.stats,
        })
    }
}

#[async_trait]
impl WorkspaceView for VfsWorkspaceView {
    fn validate_authority(&self, binding: &AttemptBinding) -> Result<()> {
        ensure!(
            *binding == self.binding,
            "provider is bound to another exact attempt"
        );
        self.authority.validate(binding)?;
        let descriptor = self.view.descriptor()?;
        ensure!(
            descriptor.view_id == binding.view_id && descriptor.owner == binding.owner,
            "VFS view owner differs from attempt authority"
        );
        Ok(())
    }
    fn bootstrap(&self, binding: &AttemptBinding, store: Arc<Store>) -> Result<ObservedTree> {
        self.validate_authority(binding)?;
        self.check_jj_store(&store)?;
        let empty = MergedTree::resolved(store.clone(), store.empty_tree_id().clone());
        pollster::block_on(self.capture(&empty, None, None))
    }
    fn validate_observation(
        &self,
        binding: &AttemptBinding,
        observation: &ViewToken,
        tree: &JjTreeIdentity,
    ) -> Result<()> {
        self.validate_authority(binding)?;
        let owned = self.read_owned(observation)?;
        ensure!(
            owned.projection.tree == *tree,
            "owned source projection does not prove this jj tree"
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
        self.validate_observation(binding, current, tree)?;
        let _lock = atomic::lock(&self.private_state.join("projection.lock"))?;
        let path = self.private_state.join("adopted.json");
        let mut adopted = if path.try_exists()? {
            atomic::read_json::<Adopted>(&path)?
        } else {
            Adopted {
                format_version: 1,
                binding: binding.clone(),
                current: None,
                release_pending: vec![],
            }
        };
        ensure!(
            adopted.format_version == 1 && adopted.binding == *binding,
            "retention state belongs to another binding"
        );
        if let (Some(previous), Some(old)) = (previous, &adopted.current) {
            ensure!(
                old == previous || old == current,
                "another WC observation was adopted; cannot discard its pin"
            );
        }
        if let Some(old) = &adopted.current {
            if old.request_nonce != current.request_nonce
                && !adopted.release_pending.contains(&old.request_nonce)
            {
                adopted.release_pending.push(old.request_nonce.clone());
            }
        }
        adopted.current = Some(current.clone());
        adopted
            .release_pending
            .retain(|nonce| nonce != &current.request_nonce);
        atomic::write_json(&path, &adopted)?;
        for nonce in &adopted.release_pending {
            // VFS retires the nonce durably BEFORE dropping only its owned pin.
            self.view.release_observation(nonce)?;
            let record_path = self.record_path(nonce)?;
            match fs::remove_file(record_path) {
                Ok(()) => File::open(self.private_state.join("observations"))?.sync_all()?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        adopted.release_pending.clear();
        atomic::write_json(&path, &adopted)
    }
    async fn observe(
        &self,
        binding: &AttemptBinding,
        previous: Option<&ViewToken>,
        previous_tree: &JjTreeIdentity,
        options: &SnapshotOptions<'_>,
        store: Arc<Store>,
    ) -> Result<ObservedTree> {
        self.validate_authority(binding)?;
        self.check_jj_store(&store)?;
        self.capture(&previous_tree.to_jj(store)?, previous, Some(options))
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
        bail!("mounted VFS root replacement/open-inode semantics are unsupported")
    }
    async fn set_sparse_patterns(
        &self,
        _: &AttemptBinding,
        _: &ViewToken,
        _: &str,
        _: &MergedTree,
        _: &[RepoPathBuf],
    ) -> Result<AppliedCheckout> {
        bail!("mounted VFS sparse checkout is unsupported")
    }
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    let created = if path.try_exists()? {
        false
    } else {
        fs::create_dir(path)?;
        true
    };
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir(),
        "provider metadata must be a real directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if created {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "provider metadata must be private and owned by this user"
        );
    }
    #[cfg(not(unix))]
    let _ = created;
    Ok(())
}
