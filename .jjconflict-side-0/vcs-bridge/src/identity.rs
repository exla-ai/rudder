use anyhow::{Result, ensure};
use rudder_engine::protocol::{ObjectKind, ObjectRef, OwnerEpoch, canonical_json};
use serde::{Deserialize, Serialize};

macro_rules! identifier {
    ($name:ident, $length:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            pub fn validate(&self) -> Result<()> {
                ensure!(
                    self.0.len() == $length
                        && self
                            .0
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    concat!(stringify!($name), " requires lowercase raw hex")
                );
                Ok(())
            }
        }
    };
}
identifier!(GitCommitId, 40);
identifier!(GitTreeId, 40);
identifier!(JjChangeId, 32);
identifier!(JjOperationId, 128);

/// Alternating add/remove/add merge terms, exactly as jj stores them. Labels
/// are empty (unlabeled) or have one entry per term. Never flatten conflicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JjTreeIdentity {
    pub terms: Vec<GitTreeId>,
    pub labels: Vec<String>,
}
impl JjTreeIdentity {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.terms.is_empty() && self.terms.len() % 2 == 1 && self.terms.len() <= 1025,
            "invalid merged-tree term count"
        );
        for id in &self.terms {
            id.validate()?;
        }
        ensure!(
            self.labels.is_empty()
                || (self.terms.len() > 1 && self.labels.len() == self.terms.len()),
            "conflict label arity mismatch"
        );
        ensure!(
            self.labels
                .iter()
                .all(|s| s.len() <= 16 * 1024 && !s.contains('\0')),
            "invalid conflict label"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitIdentity {
    pub commit: GitCommitId,
    /// Raw hexadecimal, not jj's human reverse-alphabet display encoding.
    pub change: JjChangeId,
    pub parents: Vec<GitCommitId>,
    pub tree: JjTreeIdentity,
}
impl CommitIdentity {
    pub fn validate(&self) -> Result<()> {
        self.commit.validate()?;
        self.change.validate()?;
        ensure!(self.parents.len() <= 1024, "too many commit parents");
        for parent in &self.parents {
            parent.validate()?;
        }
        self.tree.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptBinding {
    pub format_version: u32,
    pub project_id: String,
    pub owner: OwnerEpoch,
    pub run_id: String,
    pub run_epoch: u64,
    pub attempt_id: String,
    pub view_id: String,
    /// Immutable prepared seed directory, not the engine's live Git repository.
    /// The engine retains this reference for live and recoverable attempts.
    pub seed: ObjectRef,
    pub baseline: CommitIdentity,
    pub baseline_operation: JjOperationId,
    /// Canonical source projection policy, independent of recovery tree layout.
    pub projection_policy: ObjectRef,
    pub profile: ObjectRef,
}
impl AttemptBinding {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported attempt binding version"
        );
        for id in [
            &self.project_id,
            &self.run_id,
            &self.attempt_id,
            &self.view_id,
        ] {
            valid_name(id)?;
        }
        self.owner.validate()?;
        ensure!(self.run_epoch > 0, "run epoch must be positive");
        reference_kind(&self.seed, ObjectKind::Directory)?;
        reference_kind(&self.projection_policy, ObjectKind::Blob)?;
        reference_kind(&self.profile, ObjectKind::Blob)?;
        self.baseline.validate()?;
        self.baseline_operation.validate()
    }
}

/// Immutable terminal-seal proof. A live observation is deliberately a different
/// type and cannot be labeled SourceManifest/CheckpointConsistency::Quiesced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedSourceProof {
    pub binding: AttemptBinding,
    pub format_version: u32,
    pub recovery_root: ObjectRef,
    pub source: ObjectRef,
    pub environment: Option<ObjectRef>,
    pub tree: JjTreeIdentity,
}
impl SealedSourceProof {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported sealed source proof version"
        );
        self.binding.validate()?;
        self.recovery_root.validate()?;
        ensure!(
            matches!(
                self.recovery_root.kind,
                ObjectKind::Checkpoint | ObjectKind::RunnableRoot
            ),
            "recovery root must be a checkpoint or explicit runnable root"
        );
        reference_kind(&self.source, ObjectKind::SourceRoot)?;
        if let Some(environment) = &self.environment {
            reference_kind(environment, ObjectKind::EnvironmentRoot)?;
        }
        self.tree.validate()
    }
    /// Binds bytes of this record; not evidence that its claimed seal was real.
    pub fn identity(&self) -> Result<ObjectRef> {
        self.validate()?;
        Ok(ObjectRef::from_bytes(
            ObjectKind::Blob,
            &canonical_json(self)?,
        ))
    }
}

pub(crate) fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control),
        "invalid binding identifier"
    );
    Ok(())
}
pub(crate) fn reference_kind(reference: &ObjectRef, kind: ObjectKind) -> Result<()> {
    reference.validate()?;
    ensure!(reference.kind == kind, "incorrect reference kind");
    Ok(())
}

#[cfg(feature = "jj")]
impl JjTreeIdentity {
    pub fn from_jj(tree: &jj_lib::merged_tree::MergedTree) -> Self {
        use jj_lib::object_id::ObjectId;
        Self {
            terms: tree
                .tree_ids()
                .iter()
                .map(|id| GitTreeId(id.hex()))
                .collect(),
            labels: tree.labels().as_slice().to_vec(),
        }
    }
    pub fn to_jj(
        &self,
        store: std::sync::Arc<jj_lib::store::Store>,
    ) -> Result<jj_lib::merged_tree::MergedTree> {
        self.validate()?;
        let terms = self
            .terms
            .iter()
            .map(|id| Ok(jj_lib::backend::TreeId::new(hex::decode(&id.0)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(jj_lib::merged_tree::MergedTree::new(
            store,
            jj_lib::merge::Merge::from_vec(terms),
            jj_lib::conflict_labels::ConflictLabels::from_vec(self.labels.clone()),
        ))
    }
}
#[cfg(feature = "jj")]
impl CommitIdentity {
    pub fn from_jj(commit: &jj_lib::commit::Commit) -> Self {
        use jj_lib::object_id::ObjectId;
        Self {
            commit: GitCommitId(commit.id().hex()),
            change: JjChangeId(commit.change_id().hex()),
            parents: commit
                .parent_ids()
                .iter()
                .map(|id| GitCommitId(id.hex()))
                .collect(),
            tree: JjTreeIdentity::from_jj(&commit.tree()),
        }
    }
}

/// A best-effort live observation, never an authoritative quiesced checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveObservation {
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub cursor_namespace: String,
    pub request_nonce: String,
    pub consistency: LiveObservationConsistency,
    pub begin_generation: u64,
    pub end_generation: u64,
    pub directory: ObjectRef,
    pub projection_policy: ObjectRef,
    /// VFS subtree exclusions, distinct from the complete jj projection policy.
    pub exclusion_policy_digest: String,
    /// Engine-owned pin/lease; the provider retains it through state finish or
    /// explicit reconciliation and never treats generation equality as seal.
    pub retention: String,
}
impl LiveObservation {
    /// Convert only the retained live-observation token. The injected provider
    /// still applies the complete jj tracking/conflict projection; the VFS
    /// directory itself represents explicit exclusions, not a Git source seal.
    pub fn from_vfs(
        observed: &rudder_engine::vfs::VfsObservation,
        binding: &AttemptBinding,
    ) -> Result<Self> {
        ensure!(
            observed.format_version == 1,
            "unsupported VFS observation version"
        );
        ensure!(
            observed.begin.view_id == observed.view_id
                && observed.end.view_id == observed.view_id
                && observed.begin.owner == observed.owner
                && observed.end.owner == observed.owner
                && observed.begin.namespace == observed.end.namespace,
            "VFS observation cursor scope changed"
        );
        ensure!(
            matches!(
                observed.consistency,
                rudder_engine::vfs::ObservationConsistency::LiveBestEffort
            ),
            "VFS observation is not a live observation"
        );
        uuid::Uuid::parse_str(&observed.request_nonce)?;
        let token = Self {
            view_id: observed.view_id.clone(),
            owner: observed.owner.clone(),
            cursor_namespace: observed.begin.namespace.clone(),
            request_nonce: observed.request_nonce.clone(),
            consistency: LiveObservationConsistency::LiveBestEffort,
            begin_generation: observed.begin.generation,
            end_generation: observed.end.generation,
            directory: observed.directory.clone(),
            projection_policy: binding.projection_policy.clone(),
            exclusion_policy_digest: observed.exclusion_policy_digest.clone(),
            retention: observed.pin_id.clone(),
        };
        token.validate(binding)?;
        Ok(token)
    }
    pub fn validate(&self, binding: &AttemptBinding) -> Result<()> {
        crate::identity::valid_name(&self.view_id)?;
        crate::identity::valid_name(&self.retention)?;
        crate::identity::valid_name(&self.request_nonce)?;
        uuid::Uuid::parse_str(&self.cursor_namespace)?;
        ensure!(
            self.exclusion_policy_digest.len() == 64
                && self
                    .exclusion_policy_digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid VFS exclusion-policy digest"
        );
        ensure!(
            self.view_id == binding.view_id,
            "observation belongs to another view"
        );
        ensure!(
            self.owner == binding.owner,
            "observation owner epoch differs"
        );
        ensure!(
            self.begin_generation <= self.end_generation,
            "observation generation is reversed"
        );
        crate::identity::reference_kind(&self.directory, ObjectKind::Directory)?;
        ensure!(
            self.projection_policy == binding.projection_policy,
            "source projection policy differs"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveObservationConsistency {
    LiveBestEffort,
}
