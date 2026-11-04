//! Trusted off-actor adapter: engine journal -> exact pinned jj publication.

use crate::publication::*;
use anyhow::{Result, ensure};
use jj_lib::repo::RepoLoader;
use rudder_engine::journal::{PreparedVcsOperation, VcsPublicationObservation, VcsPublisher};
use rudder_engine::protocol::{ObjectKind, ObjectRef, canonical_json};
use rudder_engine::store::LocalStore;

/// The caller holds staging retention from before new proof/record writes until
/// Journal::attach_vcs_operation has retained both. This function does not
/// publish a history operation or pretend the journal and CAS share a commit.
pub fn persist_prepared_record(
    store: &LocalStore,
    staging_id: &str,
    prepared: &PreparedJjOperation,
) -> Result<PreparedVcsOperation> {
    prepared.validate()?;
    if let Some(reference) = &prepared.projection_receipt {
        crate::projection::validate_mapping_evidence(store, reference, &prepared.proof)?;
    }
    let proof_bytes = canonical_json(&prepared.proof)?;
    let proof_identity = ObjectRef::from_bytes(ObjectKind::Blob, &proof_bytes);
    store.receive_staged(staging_id, &proof_identity, &proof_bytes)?;
    let bytes = canonical_json(prepared)?;
    let record = ObjectRef::from_bytes(ObjectKind::Blob, &bytes);
    store.receive_staged(staging_id, &record, &bytes)?;
    Ok(PreparedVcsOperation {
        format_version: 1,
        record,
        operation_id: prepared.operation.0.clone(),
        expected_operation_heads: prepared
            .expected_heads
            .iter()
            .map(|id| id.0.clone())
            .collect(),
        commit_id: prepared.commit.commit.0.clone(),
        proof_identity,
    })
}

pub struct JournalJjPublisher<'a> {
    pub loader: &'a RepoLoader,
    pub store: &'a LocalStore,
}
impl JournalJjPublisher<'_> {
    fn load(&self, envelope: &PreparedVcsOperation) -> Result<PreparedJjOperation> {
        let bytes = self.store.read_object(&envelope.record)?;
        let record: PreparedJjOperation = serde_json::from_slice(&bytes)?;
        record.validate()?;
        if let Some(reference) = &record.projection_receipt {
            crate::projection::validate_mapping_evidence(self.store, reference, &record.proof)?;
        }
        ensure!(
            canonical_json(&record)? == bytes,
            "prepared jj record is not canonical"
        );
        ensure!(
            record.operation.0 == envelope.operation_id
                && record
                    .expected_heads
                    .iter()
                    .map(|id| &id.0)
                    .eq(envelope.expected_operation_heads.iter())
                && record.commit.commit.0 == envelope.commit_id
                && record.proof.identity()? == envelope.proof_identity,
            "prepared jj envelope differs from its immutable record"
        );
        Ok(record)
    }
}
impl VcsPublisher for JournalJjPublisher<'_> {
    fn inspect(&mut self, envelope: &PreparedVcsOperation) -> Result<VcsPublicationObservation> {
        let record = self.load(envelope)?;
        Ok(convert(pollster::block_on(inspect_operation(
            self.loader,
            &record,
        ))?))
    }
    fn publish(&mut self, envelope: &PreparedVcsOperation) -> Result<VcsPublicationObservation> {
        let record = self.load(envelope)?;
        Ok(convert(pollster::block_on(publish_operation(
            self.loader,
            &record,
        ))?))
    }
}
fn convert(state: PublicationState) -> VcsPublicationObservation {
    match state {
        PublicationState::NotPublished => VcsPublicationObservation::NotPublished,
        PublicationState::Published => VcsPublicationObservation::Published,
        PublicationState::HeadChanged { actual_heads } => VcsPublicationObservation::HeadChanged {
            actual_heads: actual_heads.into_iter().map(|id| id.0).collect(),
        },
        PublicationState::Uncertain { message } => VcsPublicationObservation::Uncertain { message },
    }
}
