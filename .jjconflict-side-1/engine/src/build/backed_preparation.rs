//! Preparation of an already-built local/baked content union. These are
//! storage/index owners, never Root, SDK, project, lease or execution grants.
//! Keep the whole original carrier on errors, panic and unknown outcomes.
use super::*;
use crate::{execution::finalization_budget::FinalizationConstraint,
    store::{backed_data::BakedInputsBindingData, backed_inputs::VerifiedBakedInputs,
        backed_retention::{BackedPinOwner, BackedPublication, BackedRead}}};
use std::sync::Arc;

pub const BACKED_PREPARATION_CATEGORY: &str = "environment-backed-v1";
pub(in crate::build) const KEY_SCHEMA: &str = "prepared-environment-backed-key-v1";
pub(in crate::build) const RECEIPT_SCHEMA: &str = "prepared-environment-backed-receipt-v1";

/// Serializable DATA in a distinct category. It cannot stand in for either an
/// ordinary LocalStore PreparedEnvironment or an actual live backing owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackedPreparedEnvironment {
    pub format_version: u32,
    pub key: ObjectRef,
    pub root: ObjectRef,
    pub receipt: ObjectRef,
    pub backings: Vec<BakedInputsBindingData>,
}

/// Borrowed only from a successfully completed original carrier. This keeps
/// the actual index, sources and backed pin, and cannot be minted from DATA.
pub struct BackedPreparationAcknowledgment {
    pub(in crate::build) prepared: BackedPreparedEnvironment,
    pub(in crate::build) reused: bool,
    pub(in crate::build) index: Arc<BuildIndex>,
    pub(in crate::build) sources: Vec<Arc<VerifiedBakedInputs>>,
    pub(in crate::build) retention: Arc<BackedPinOwner>,
}
impl BackedPreparationAcknowledgment {
    pub fn prepared(&self) -> &BackedPreparedEnvironment { &self.prepared }
    pub fn reused(&self) -> bool { self.reused }
    pub fn retention(&self) -> &Arc<BackedPinOwner> { &self.retention }
}

pub(in crate::build) struct PriorAcknowledgment {
    pub prepared: BackedPreparedEnvironment,
    pub index: Arc<BuildIndex>,
    pub sources: Vec<Arc<VerifiedBakedInputs>>,
    pub retention: Arc<BackedPinOwner>,
}

/// A dedicated original transaction and original key/store custody survive
/// every Err. Successful reads may be released only after their synchronous
/// completion; failed reads, publication ACKs and SQL intents remain retained.
#[must_use = "retain the whole backed preparation carrier on every outcome"]
pub struct RetainedBackedPreparation {
    pub(in crate::build) core: RetainedPreparation,
    pub(in crate::build) sources: Vec<Arc<VerifiedBakedInputs>>,
    pub(in crate::build) prior: Option<PriorAcknowledgment>,
    pub(in crate::build) bindings: Vec<BakedInputsBindingData>,
    pub(in crate::build) prepared: Option<BackedPreparedEnvironment>,
    pub(in crate::build) publication: Option<BackedPublication>,
    pub(in crate::build) retention: Option<Arc<BackedPinOwner>>,
    pub(in crate::build) read: Option<BackedRead>,
    pub(in crate::build) metadata: BTreeMap<ObjectRef, Vec<u8>>,
    pub(in crate::build) metadata_bytes: u64,
    pub(in crate::build) ack: Option<BackedPreparationAcknowledgment>,
    #[cfg(test)] pub(in crate::build) fault: Option<TestPoint>,
}
impl RetainedBackedPreparation {
    pub fn status(&self) -> RetainedPreparationStatus { self.core.status() }
    pub fn original_constraint_data(&self) -> FinalizationConstraint { self.core.original_constraint_data() }
    pub fn key_data(&self) -> Option<&ObjectRef> { self.prepared.as_ref().map(|p| &p.key) }
    pub fn staging_id_data(&self) -> Option<&str> { self.core.staging_id_data() }
    pub fn publication(&self) -> Option<&BackedPublication> { self.publication.as_ref() }
    pub fn failure(&self) -> Option<&anyhow::Error> { self.core.failure() }
    pub fn acknowledgment(&self) -> Result<&BackedPreparationAcknowledgment> {
        ensure!(self.status() == RetainedPreparationStatus::Acknowledged,
            "original backed preparation ACK absent/unknown");
        self.ack.as_ref().context("original backed preparation ACK absent")
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::build) enum TestPoint {
    AfterMetadata, AfterRetention, AfterInsert, AfterCommit,
    CancelAfterRetention, CancelAfterCommit, PanicAfterRetention, CommitHookRefusal,
}
