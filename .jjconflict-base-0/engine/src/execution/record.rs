//! Versioned audit data, never reconstructed process or finalization authority.
//! V1 is the original flat record. Its serializer and envelope checksum remain
//! byte-compatible. V2 explicitly distinguishes local and partial artifacts;
//! serialized partial data cannot stand in for a VfsCheckpoint or complete pin.
use super::{ExecutionPhase, StopAndCheckpoint};
use crate::{
    protocol::{ObjectKind, ObjectRef, OwnerEpoch},
    store::ObjectSetReceipt,
    vfs::{rpc::ObservationBinding, ObservationConsistency, PartialCaptureReceipt, VfsCheckpoint},
};
use anyhow::{ensure, Result};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const MAX_EXECUTION_RECORD_BYTES: usize = 1024 * 1024;
pub const MAX_RECORD_V2_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone)]
pub enum ExecutionRecord {
    V1(ExecutionRecordV1),
    V2(ExecutionRecordV2),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRecordV1 {
    #[serde(deserialize_with = "deserialize_version1")]
    pub format_version: u32,
    pub execution_id: String,
    pub operation_id: String,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub request_digest: String,
    pub phase: ExecutionPhase,
    pub supervisor_pid: u32,
    pub agent_exit: Option<i32>,
    pub checkpoint: Option<VfsCheckpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionIdentityV2 {
    pub project_id: String,
    pub request_id: String,
    pub run_id: String,
    pub run_epoch: u64,
    pub execution_id: String,
    pub operation_id: String,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub request_digest: String,
    pub supervisor_pid: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRecordV2 {
    #[serde(deserialize_with = "deserialize_version2")]
    pub format_version: u32,
    pub identity: ExecutionIdentityV2,
    pub state: ExecutionStateV2,
}
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionStateV2 {
    Preparing,
    Running,
    Orphaned,
    Checkpointed {
        agent_exit: i32,
        checkpoint: CheckpointArtifactV2,
    },
}
impl<'de> Deserialize<'de> for ExecutionStateV2 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        // Serde's internally-tagged unit visitor ignores residual map fields.
        // Empty struct variants preserve the public unit API and reject them.
        #[derive(Deserialize)]
        #[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Preparing {},
            Running {},
            Orphaned {},
            Checkpointed {
                agent_exit: i32,
                checkpoint: CheckpointArtifactV2,
            },
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Preparing {} => Self::Preparing,
            Wire::Running {} => Self::Running,
            Wire::Orphaned {} => Self::Orphaned,
            Wire::Checkpointed {
                agent_exit,
                checkpoint,
            } => Self::Checkpointed {
                agent_exit,
                checkpoint,
            },
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CheckpointArtifactV2 {
    CompleteLocal(VfsCheckpoint),
    PartialRemote(QuiescedPartialCheckpointV1),
}
/// Durable audit DATA only. A matching cursor, these fields, or deserialization
/// do not prove physical writer drain, a live source grant or successful fsync.
/// Only the executor's actual drain/frozen-view/ACK join may publish this
/// artifact as a finalized result. The two evidence Blobs have their own
/// retention receipt; the artifact never retains or hashes itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuiescedPartialCheckpointV1 {
    pub format_version: u32,
    pub capture: PartialCaptureReceipt,
    pub stop: StopAndCheckpoint,
    pub observation: ObservationBinding,
    pub execution_request_digest: String,
    pub freeze_intent: ObjectRef,
    pub drain_record: ObjectRef,
    pub evidence_retention: ObjectSetReceipt,
}
#[derive(Debug, Clone, Copy)]
pub enum CheckpointRef<'a> {
    CompleteLocal(&'a VfsCheckpoint),
    PartialRemote(&'a QuiescedPartialCheckpointV1),
}

impl ExecutionRecord {
    pub fn format_version(&self) -> u32 {
        match self {
            Self::V1(record) => record.format_version,
            Self::V2(record) => record.format_version,
        }
    }
    pub fn execution_id(&self) -> &str {
        match self {
            Self::V1(record) => &record.execution_id,
            Self::V2(record) => &record.identity.execution_id,
        }
    }
    pub fn operation_id(&self) -> &str {
        match self {
            Self::V1(record) => &record.operation_id,
            Self::V2(record) => &record.identity.operation_id,
        }
    }
    pub fn view_id(&self) -> &str {
        match self {
            Self::V1(record) => &record.view_id,
            Self::V2(record) => &record.identity.view_id,
        }
    }
    pub fn owner(&self) -> &OwnerEpoch {
        match self {
            Self::V1(record) => &record.owner,
            Self::V2(record) => &record.identity.owner,
        }
    }
    pub fn request_digest(&self) -> &str {
        match self {
            Self::V1(record) => &record.request_digest,
            Self::V2(record) => &record.identity.request_digest,
        }
    }
    pub fn supervisor_pid(&self) -> u32 {
        match self {
            Self::V1(record) => record.supervisor_pid,
            Self::V2(record) => record.identity.supervisor_pid,
        }
    }
    pub fn phase(&self) -> ExecutionPhase {
        match self {
            Self::V1(record) => record.phase,
            Self::V2(record) => match &record.state {
                ExecutionStateV2::Preparing => ExecutionPhase::Preparing,
                ExecutionStateV2::Running => ExecutionPhase::Running,
                ExecutionStateV2::Orphaned => ExecutionPhase::Orphaned,
                ExecutionStateV2::Checkpointed { .. } => ExecutionPhase::Checkpointed,
            },
        }
    }
    pub fn agent_exit(&self) -> Option<i32> {
        match self {
            Self::V1(record) => record.agent_exit,
            Self::V2(record) => match &record.state {
                ExecutionStateV2::Checkpointed { agent_exit, .. } => Some(*agent_exit),
                _ => None,
            },
        }
    }
    /// Audit-data discrimination only, never a generic complete-pin getter.
    pub fn checkpoint(&self) -> Option<CheckpointRef<'_>> {
        match self {
            Self::V1(record) => record.checkpoint.as_ref().map(CheckpointRef::CompleteLocal),
            Self::V2(record) => match &record.state {
                ExecutionStateV2::Checkpointed {
                    checkpoint: CheckpointArtifactV2::CompleteLocal(checkpoint),
                    ..
                } => Some(CheckpointRef::CompleteLocal(checkpoint)),
                ExecutionStateV2::Checkpointed {
                    checkpoint: CheckpointArtifactV2::PartialRemote(checkpoint),
                    ..
                } => Some(CheckpointRef::PartialRemote(checkpoint)),
                _ => None,
            },
        }
    }
    pub fn validate_metadata(&self) -> Result<()> {
        ensure!(
            matches!(self, Self::V1(record) if record.format_version == 1)
                || matches!(self, Self::V2(record) if record.format_version == 2),
            "execution record variant/version differs"
        );
        for id in [self.execution_id(), self.operation_id(), self.view_id()] {
            canonical_uuid(id)?;
        }
        self.owner().validate()?;
        digest(self.request_digest())?;
        if let Some(checkpoint) = self.checkpoint() {
            match checkpoint {
                CheckpointRef::CompleteLocal(checkpoint) => validate_complete(checkpoint)?,
                CheckpointRef::PartialRemote(checkpoint) => checkpoint.validate_data()?,
            }
        }
        if let Self::V2(record) = self {
            let identity = &record.identity;
            identity.validate()?;
            if let Some(CheckpointRef::PartialRemote(checkpoint)) = self.checkpoint() {
                ensure!(
                    checkpoint.execution_request_digest == identity.request_digest
                        && checkpoint.capture.attempt.project_id == identity.project_id
                        && checkpoint.capture.attempt.run_id == identity.run_id
                        && checkpoint.capture.attempt.run_epoch == identity.run_epoch
                        && checkpoint.capture.attempt.execution_id == identity.execution_id
                        && checkpoint.capture.attempt.operation_id == identity.operation_id
                        && checkpoint.capture.attempt.view_id == identity.view_id
                        && checkpoint.capture.attempt.owner == identity.owner,
                    "partial artifact differs from execution identity"
                );
            }
            ensure!(
                serde_json::to_vec(record)?.len() <= MAX_RECORD_V2_BYTES,
                "record2 exceeds32KiB"
            );
        }
        Ok(())
    }
    /// Validates terminal DATA shape only; callers still need physical proof.
    pub fn checkpointed_data(&self) -> Result<CheckpointRef<'_>> {
        self.validate_metadata()?;
        ensure!(
            self.phase() == ExecutionPhase::Checkpointed && self.agent_exit().is_some(),
            "execution lacks terminal checkpoint data"
        );
        self.checkpoint()
            .ok_or_else(|| anyhow::anyhow!("execution lacks checkpoint data"))
    }
    pub(super) fn set_running(&mut self) -> Result<()> {
        ensure!(
            self.phase() == ExecutionPhase::Preparing,
            "execution is not preparing"
        );
        match self {
            Self::V1(record) => record.phase = ExecutionPhase::Running,
            Self::V2(record) => record.state = ExecutionStateV2::Running,
        }
        Ok(())
    }
    pub(super) fn set_orphaned(&mut self) -> Result<()> {
        ensure!(
            self.phase() != ExecutionPhase::Checkpointed,
            "completed execution data cannot be overwritten as orphaned"
        );
        match self {
            Self::V1(record) => record.phase = ExecutionPhase::Orphaned,
            Self::V2(record) => record.state = ExecutionStateV2::Orphaned,
        }
        Ok(())
    }
    pub(super) fn set_complete_local(
        &mut self,
        checkpoint: VfsCheckpoint,
        agent_exit: i32,
    ) -> Result<()> {
        ensure!(
            self.phase() == ExecutionPhase::Running,
            "only a running execution may commit a local checkpoint"
        );
        validate_complete(&checkpoint)?;
        match self {
            Self::V1(record) => {
                record.phase = ExecutionPhase::Checkpointed;
                record.agent_exit = Some(agent_exit);
                record.checkpoint = Some(checkpoint);
            }
            Self::V2(record) => {
                record.state = ExecutionStateV2::Checkpointed {
                    agent_exit,
                    checkpoint: CheckpointArtifactV2::CompleteLocal(checkpoint),
                }
            }
        }
        Ok(())
    }
    pub(super) fn set_partial_remote(
        &mut self,
        checkpoint: QuiescedPartialCheckpointV1,
        agent_exit: i32,
    ) -> Result<()> {
        ensure!(
            self.phase() == ExecutionPhase::Running,
            "only a running execution may commit a partial checkpoint"
        );
        checkpoint.validate_data()?;
        let Self::V2(record) = self else {
            anyhow::bail!("partial checkpoint requires record2");
        };
        record.state = ExecutionStateV2::Checkpointed {
            agent_exit,
            checkpoint: CheckpointArtifactV2::PartialRemote(checkpoint),
        };
        self.validate_metadata()
    }
}
impl ExecutionIdentityV2 {
    pub fn validate(&self) -> Result<()> {
        for value in [&self.project_id, &self.run_id, &self.owner.owner] {
            bounded_identity(value)?;
        }
        for id in [
            &self.request_id,
            &self.execution_id,
            &self.operation_id,
            &self.view_id,
        ] {
            canonical_uuid(id)?;
        }
        self.owner.validate()?;
        ensure!(
            self.owner.epoch <= i64::MAX as u64
                && self.run_epoch > 0
                && self.run_epoch <= i64::MAX as u64
                && self.supervisor_pid > 0,
            "invalid execution2 epoch or supervisor"
        );
        digest(&self.request_digest)
    }
}
impl QuiescedPartialCheckpointV1 {
    pub fn validate_data(&self) -> Result<()> {
        let capture = &self.capture;
        let attempt = &capture.attempt;
        self.observation.validate()?;
        canonical_uuid(&self.stop.request_id)?;
        canonical_uuid(&capture.request_nonce)?;
        digest(&self.execution_request_digest)?;
        digest(&capture.exclusion_policy_digest)?;
        for value in [
            &attempt.project_id,
            &attempt.run_id,
            &attempt.owner.owner,
            &capture.base.root_id,
        ] {
            bounded_identity(value)?;
        }
        capture
            .base
            .scope
            .digest()
            .map_err(|_| anyhow::anyhow!("invalid partial root scope"))?;
        kind(&capture.base.root, ObjectKind::RunnableRoot)?;
        kind(&capture.retention.plan, ObjectKind::Blob)?;
        kind(&self.freeze_intent, ObjectKind::Blob)?;
        kind(&self.drain_record, ObjectKind::Blob)?;
        self.validate_evidence_receipt()?;
        let local = &capture.retention.local;
        bounded_identity(&local.pin_id)?;
        canonical_uuid(&local.staging_id)?;
        digest(&local.set_digest)?;
        ensure!(
            self.format_version == 1
                && capture.format_version == 1
                && capture.retention.format_version == 1
                && local.format_version == 1
                && local.generation > 0
                && local.generation <= i64::MAX as u64
                && local.object_count > 0
                && capture.head_sequence > 0
                && capture.consistency == ObservationConsistency::LiveBestEffort
                && capture.begin == capture.end
                && capture.view_id == attempt.view_id
                && capture.owner == attempt.owner
                && capture.begin.view_id == capture.view_id
                && capture.begin.owner == capture.owner
                && capture.begin.namespace == self.observation.namespace
                && capture.exclusion_policy_digest == self.observation.exclusion_policy_digest
                && self.observation.view_id == attempt.view_id
                && self.observation.owner == attempt.owner
                && self.observation.attempt.execution_id == attempt.execution_id
                && self.observation.attempt.operation_id == attempt.operation_id
                && self.stop.execution_id == attempt.execution_id
                && self.stop.operation_id == attempt.operation_id
                && self.stop.view_id == attempt.view_id
                && self.stop.owner == attempt.owner
                && self.stop.run_id == attempt.run_id
                && self.stop.run_epoch == attempt.run_epoch
                && capture.request_nonce == self.stop.request_id
                && attempt.run_epoch > 0
                && attempt.run_epoch <= i64::MAX as u64,
            "partial checkpoint audit binding differs"
        );
        if let Some(previous) = &capture.previous_head {
            bounded_identity(previous)?;
        }
        Ok(())
    }
    pub(super) fn evidence_refs(&self) -> Result<Vec<ObjectRef>> {
        kind(&self.freeze_intent, ObjectKind::Blob)?;
        kind(&self.drain_record, ObjectKind::Blob)?;
        ensure!(
            self.freeze_intent != self.drain_record,
            "partial evidence identities overlap"
        );
        let mut refs = vec![self.freeze_intent.clone(), self.drain_record.clone()];
        refs.sort();
        Ok(refs)
    }
    fn validate_evidence_receipt(&self) -> Result<()> {
        let refs = self.evidence_refs()?;
        let receipt = &self.evidence_retention;
        bounded_identity(&receipt.pin_id)?;
        canonical_uuid(&receipt.staging_id)?;
        let total = self
            .freeze_intent
            .size_bytes
            .checked_add(self.drain_record.size_bytes)
            .ok_or_else(|| anyhow::anyhow!("partial evidence byte count overflow"))?;
        ensure!(
            self.freeze_intent.size_bytes <= 64 * 1024
                && self.drain_record.size_bytes <= 32 * 1024
                && receipt.format_version == 1
                && receipt.generation > 0
                && receipt.generation <= i64::MAX as u64
                && receipt.object_count == 2
                && receipt.total_bytes == total
                && receipt.set_digest
                    == hex::encode(Sha256::digest(crate::protocol::canonical_json(&refs)?)),
            "partial evidence receipt differs from the exact two Blobs"
        );
        Ok(())
    }
}
impl Serialize for ExecutionRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.validate_metadata()
            .map_err(serde::ser::Error::custom)?;
        match self {
            Self::V1(record) => record.serialize(serializer),
            Self::V2(record) => record.serialize(serializer),
        }
    }
}
impl<'de> Deserialize<'de> for ExecutionRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        if raw.get().len() > MAX_EXECUTION_RECORD_BYTES {
            return Err(de::Error::custom("execution record exceeds bound"));
        }
        #[derive(Deserialize)]
        struct Version {
            format_version: u32,
        }
        let version: Version = serde_json::from_str(raw.get())
            .map_err(|_| de::Error::custom("invalid execution record version"))?;
        let record = match version.format_version {
            1 => Self::V1(
                serde_json::from_str(raw.get())
                    .map_err(|_| de::Error::custom("invalid execution record1 data"))?,
            ),
            2 => {
                if raw.get().len() > MAX_RECORD_V2_BYTES {
                    return Err(de::Error::custom("record2 exceeds32KiB"));
                }
                Self::V2(
                    serde_json::from_str(raw.get())
                        .map_err(|_| de::Error::custom("invalid execution record2 data"))?,
                )
            }
            _ => return Err(de::Error::custom("unsupported execution record version")),
        };
        record.validate_metadata().map_err(de::Error::custom)?;
        Ok(record)
    }
}

/// Successful State fsync and custody checks, not kernel/quiescence proof.
/// No Deserialize/Clone/public constructor; the executor must separately join
/// its actual drain witness before sending an authenticated shutdown result.
pub struct CommittedExecutionRecord {
    record: ExecutionRecord,
    checksum: String,
}
impl CommittedExecutionRecord {
    pub fn record(&self) -> &ExecutionRecord {
        &self.record
    }
    pub fn checksum(&self) -> &str {
        &self.checksum
    }
    pub(super) fn new(record: ExecutionRecord) -> Result<Self> {
        let checksum = hex::encode(Sha256::digest(crate::protocol::canonical_json(&record)?));
        Ok(Self { record, checksum })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    record: ExecutionRecord,
    checksum: String,
}
pub(super) fn encode_envelope(record: &ExecutionRecord) -> Result<Vec<u8>> {
    let checksum = hex::encode(Sha256::digest(crate::protocol::canonical_json(record)?));
    let bytes = crate::protocol::canonical_json(&Envelope {
        record: record.clone(),
        checksum,
    })?;
    ensure!(
        bytes.len() <= MAX_EXECUTION_RECORD_BYTES,
        "execution record envelope exceeds bound"
    );
    Ok(bytes)
}
pub(super) fn decode_envelope(bytes: &[u8]) -> Result<ExecutionRecord> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_EXECUTION_RECORD_BYTES,
        "execution record envelope exceeds bound"
    );
    let envelope: Envelope = serde_json::from_slice(bytes)?;
    ensure!(
        hex::encode(Sha256::digest(crate::protocol::canonical_json(
            &envelope.record
        )?)) == envelope.checksum,
        "execution record checksum differs"
    );
    Ok(envelope.record)
}
fn validate_complete(checkpoint: &VfsCheckpoint) -> Result<()> {
    ensure!(
        checkpoint.format_version == 1,
        "unsupported complete checkpoint data"
    );
    kind(&checkpoint.directory, ObjectKind::Directory)?;
    kind(&checkpoint.hardlinks, ObjectKind::Blob)
}
fn kind(reference: &ObjectRef, expected: ObjectKind) -> Result<()> {
    reference.validate()?;
    ensure!(
        reference.kind == expected,
        "execution artifact reference kind differs"
    );
    Ok(())
}
fn deserialize_version1<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let value = u32::deserialize(deserializer)?;
    if value != 1 {
        return Err(de::Error::custom("expected execution record1"));
    }
    Ok(value)
}
fn deserialize_version2<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let value = u32::deserialize(deserializer)?;
    if value != 2 {
        return Err(de::Error::custom("expected execution record2"));
    }
    Ok(value)
}
fn canonical_uuid(value: &str) -> Result<()> {
    ensure!(
        Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value),
        "noncanonical execution UUID"
    );
    Ok(())
}
fn digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid execution digest"
    );
    Ok(())
}
fn bounded_identity(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 512
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "invalid execution identity"
    );
    Ok(())
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
