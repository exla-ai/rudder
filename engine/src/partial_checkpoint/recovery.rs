use super::*;

/// Bound DATA handle only. Persist the exact original Q and template along
/// with this intent before effects; these four fields cannot recreate inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialCheckpointDataIntent {
    pub format_version: u32,
    pub staging_id: String,
    pub publication_id: String,
    pub publication: ObjectRef,
}
impl PartialCheckpointDataIntent {
    pub fn validate_data(&self) -> Result<()> {
        ensure!(self.format_version == 1, "unsupported partial DATA intent");
        canonical_uuid(&self.staging_id)?;
        publication_id(&self.publication_id)?;
        kind(
            &self.publication,
            ObjectKind::ActionResult,
            MAX_DOCUMENT_BYTES as u64,
        )?;
        ensure!(
            canonical_json(self)?.len() <= MAX_DOCUMENT_BYTES,
            "partial DATA intent exceeds64KiB"
        );
        Ok(())
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let intent: Self = decode(bytes, MAX_DOCUMENT_BYTES)?;
        intent.validate_data()?;
        Ok(intent)
    }
}

/// Observed CURRENT same-operation DATA retention, not a recovered first ACK,
/// producer ownership/cleanup capability or a physical/current execution grant.
/// A pre-ACK intent has no expected generation. If a conservative crash-stage
/// was explicitly reused with the same UUID and exact objects, the first ACK
/// is unknowable here. Known receipts still require their exact generation.
/// A transfer must always acquire/ACK its own clone before exposing reads.
pub struct CurrentPartialCheckpointDataRetention {
    pub(super) receipt: PartialCheckpointDataReceipt,
}
impl CurrentPartialCheckpointDataRetention {
    pub fn publication_id(&self) -> &str {
        &self.receipt.publication_id
    }
    pub fn publication(&self) -> &ObjectRef {
        &self.receipt.publication
    }
    /// Opaque current DATA input for pin_partial_checkpoint, never an original
    /// producer ACK or permission for automatic producer-pin cleanup.
    pub fn transfer_receipt_json(&self) -> Result<String> {
        self.receipt.validate_data()?;
        let json = serde_json::to_string(&self.receipt)?;
        ensure!(
            json.len() <= MAX_DOCUMENT_BYTES,
            "partial DATA receipt exceeds64KiB"
        );
        Ok(json)
    }
}

impl PreparedPartialCheckpointData {
    pub fn intent(&self) -> Result<PartialCheckpointDataIntent> {
        let intent = PartialCheckpointDataIntent {
            format_version: 1,
            staging_id: self.staging_id.clone(),
            publication_id: self.publication.payload.publication_id.clone(),
            publication: self.publication_reference()?,
        };
        intent.validate_data()?;
        Ok(intent)
    }
}

/// Blocking read-only DATA reconciliation after a lost ACK/process restart.
/// Uses separate recovery-owned GC staging, never renews/revives the original
/// lease and never writes wrappers or releases/adopts producer ownership.
/// Caller must durably retain the original exact Q/template along with intent.
pub fn reconcile_partial_checkpoint_data(
    store: &LocalStore,
    intent: &PartialCheckpointDataIntent,
    q: &QuiescedPartialCheckpointV1,
    template: &PartialCheckpointTemplateV1,
) -> Result<Option<CurrentPartialCheckpointDataRetention>> {
    intent.validate_data()?;
    with_read_staging(store, || {
        let prepared = prepare_from_data(store, &intent.staging_id, q, template)?;
        ensure!(
            prepared.intent()? == *intent,
            "partial DATA intent differs from original Q/template"
        );
        prepared.read_current_retention(store)
    })
}

pub(super) fn with_read_staging<T>(
    store: &LocalStore,
    read: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let recovery = store.begin_staging(STAGING_TTL)?;
    let result = read();
    // All reads above are synchronous and have actually settled. A failed
    // cleanup leaves only this distinct owned lease conservative until TTL.
    let cleanup = store.abandon_staging(&recovery.id);
    let result = result?;
    cleanup.context("partial DATA recovery staging cleanup was not confirmed")?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_fence_survives_actual_original_staging_consumption_until_reads_settle() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalStore::open(root.path()).unwrap();
        let original = store.begin_staging(STAGING_TTL).unwrap();
        let reference = store.put_blob(b"producer read custody").unwrap();
        let receipt = with_read_staging(&store, || {
            let receipt = store.retain_object_set(
                &original.id,
                "producer-read-test",
                &[reference.clone()],
            )?;
            let state = store.retention()?;
            ensure!(
                !state.staging.contains_key(&original.id) && state.staging.len() == 1,
                "only actual read fence remains"
            );
            ensure!(
                store.gc(Duration::ZERO)?.blocked_by_staging,
                "GC escaped read custody after producer ACK"
            );
            store.read_object(&reference)?;
            Ok(receipt)
        })
        .unwrap();
        assert!(store.retention().unwrap().staging.is_empty());
        assert!(!store.gc(Duration::ZERO).unwrap().blocked_by_staging);
        assert_eq!(store.object_set_refs(&receipt).unwrap(), vec![reference]);
    }
}
