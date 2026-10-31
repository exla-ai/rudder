//! Retained partial-checkpoint DATA publication, never execution/history authority.
//!
//! The explicit ActionResult edges cover the current runnable byte graph and
//! opaque audit Blobs without pulling old roots or a private VCS seed into it.
//! Preparation, validation and retention are bounded blocking off-actor work.
//! Neither historical Q bytes nor these receipts reconstruct a drain, current
//! grant, complete local pin, verified jj projection or accepted source.
use crate::{
    delta::{self, DeltaPlan},
    execution::{QuiescedPartialCheckpointV1, StopAndCheckpoint},
    protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef, OwnerEpoch},
    snapshot::{self, RunnableRoot, SourceV2},
    store::{LocalStore, ObjectSetReceipt},
    vfs::{
        partial_rpc::{PartialBinding, PROTOCOL},
        remote_launch::RemoteLaunchRequest,
        HardlinkTopology, RemoteFreezeIntent,
    },
    workspace::WorkspaceProfile,
};
use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    time::{Duration, Instant},
};
use uuid::Uuid;

mod recovery;
pub use recovery::{
    reconcile_partial_checkpoint_data, CurrentPartialCheckpointDataRetention,
    PartialCheckpointDataIntent,
};

pub const PURPOSE: &str = "partial-checkpoint-data-v1";
pub const SOURCE_ROLE: &str = "quiesced_source_complement_data";
pub const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
pub const MAX_Q_BYTES: usize = 32 * 1024;
pub const MAX_FREEZE_BYTES: usize = 64 * 1024;
pub const MAX_DRAIN_BYTES: usize = 32 * 1024;
pub const MAX_OBJECTS: usize = 16_384;
pub const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const STAGING_TTL: Duration = Duration::from_secs(600);
const VALIDATION_TIME: Duration = Duration::from_secs(30);

/// Prior identities/descriptors only. The private seed is not read, retained
/// or authenticated here. The template creates no new accepted source/seed pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialCheckpointTemplateV1 {
    pub format_version: u32,
    pub selection: RemoteLaunchRequest,
    pub base_runnable: RunnableRoot,
    pub base_source: SourceV2,
    pub profile: WorkspaceProfile,
}
impl PartialCheckpointTemplateV1 {
    pub fn validate_data(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported partial checkpoint template"
        );
        self.selection.validate()?;
        self.profile.validate()?;
        let root = &self.base_runnable;
        let source = &self.base_source;
        // Reuse the actual canonical schema readers, not just matching labels.
        snapshot::decode_runnable(&canonical_json(root)?)?;
        snapshot::decode_source(&canonical_json(source)?)?;
        ensure!(
            self.profile == WorkspaceProfile::source_only()
                && reference(ObjectKind::RunnableRoot, root)? == self.selection.root.root
                && reference(ObjectKind::SourceRoot, source)? == root.source
                && root.source == self.selection.accepted_source
                && reference(ObjectKind::Blob, &self.profile)? == root.profile
                && root.profile == self.selection.observation.profile
                && root.environment.is_none()
                && source.directory == root.view_root
                && source.projection_policy == self.selection.projection_policy
                && source.projection_policy.size_bytes <= snapshot::MAX_POLICY_BYTES
                && root.profile.size_bytes <= snapshot::MAX_POLICY_BYTES
                && root.hardlinks == empty_topology()?,
            "partial checkpoint requires the exact source-only base/template"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_DOCUMENT_BYTES,
            "partial checkpoint template exceeds64KiB"
        );
        Ok(())
    }
}

/// Cycle-free publication identity. Only publication_id is excluded from its
/// own hash. Transfer pins, staging IDs and clone generations never enter it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialCheckpointPayloadV1 {
    pub format_version: u32,
    pub kind: String,
    pub source_role: String,
    pub publication_id: String,
    pub template: PartialCheckpointTemplateV1,
    pub q: ObjectRef,
    pub source: ObjectRef,
    pub runnable: ObjectRef,
    pub plan: ObjectRef,
    pub freeze_intent: ObjectRef,
    pub drain_record: ObjectRef,
}
impl PartialCheckpointPayloadV1 {
    pub fn identity_bytes(&self) -> Result<Vec<u8>> {
        #[derive(Serialize)]
        struct Identity<'a> {
            format_version: u32,
            kind: &'a str,
            source_role: &'a str,
            template: &'a PartialCheckpointTemplateV1,
            q: &'a ObjectRef,
            source: &'a ObjectRef,
            runnable: &'a ObjectRef,
            plan: &'a ObjectRef,
            freeze_intent: &'a ObjectRef,
            drain_record: &'a ObjectRef,
        }
        canonical_json(&Identity {
            format_version: self.format_version,
            kind: &self.kind,
            source_role: &self.source_role,
            template: &self.template,
            q: &self.q,
            source: &self.source,
            runnable: &self.runnable,
            plan: &self.plan,
            freeze_intent: &self.freeze_intent,
            drain_record: &self.drain_record,
        })
    }
    pub fn expected_publication_id(&self) -> Result<String> {
        Ok(format!(
            "pcd1-{}",
            hex::encode(Sha256::digest(self.identity_bytes()?))
        ))
    }
    pub fn references(&self) -> Result<Vec<ObjectRef>> {
        let refs = vec![
            self.source.clone(),
            self.runnable.clone(),
            self.q.clone(),
            self.plan.clone(),
            self.freeze_intent.clone(),
            self.drain_record.clone(),
        ];
        let refs = bounded_objects(&refs)?;
        ensure!(
            refs.len() == 6,
            "partial publication requires six distinct strong refs"
        );
        Ok(refs)
    }
    pub fn validate_data(&self) -> Result<()> {
        ensure!(
            self.format_version == 1 && self.kind == PURPOSE && self.source_role == SOURCE_ROLE,
            "unsupported partial checkpoint purpose/source role"
        );
        self.template.validate_data()?;
        kind(&self.q, ObjectKind::Blob, MAX_Q_BYTES as u64)?;
        kind(
            &self.source,
            ObjectKind::SourceRoot,
            snapshot::MAX_MANIFEST_BYTES as u64,
        )?;
        kind(
            &self.runnable,
            ObjectKind::RunnableRoot,
            snapshot::MAX_MANIFEST_BYTES as u64,
        )?;
        kind(&self.plan, ObjectKind::Blob, 4 * 1024 * 1024)?;
        kind(
            &self.freeze_intent,
            ObjectKind::Blob,
            MAX_FREEZE_BYTES as u64,
        )?;
        kind(&self.drain_record, ObjectKind::Blob, MAX_DRAIN_BYTES as u64)?;
        self.references()?;
        ensure!(
            self.publication_id == self.expected_publication_id()?,
            "partial checkpoint immutable publication id differs"
        );
        Ok(())
    }
}

/// Standard ActionResult strong edges, with a strict purpose-specific payload.
/// Template references are prior audit identities, not ancestry/seed edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialCheckpointPublicationV1 {
    pub format_version: u32,
    pub references: Vec<ObjectRef>,
    pub payload: PartialCheckpointPayloadV1,
}
impl PartialCheckpointPublicationV1 {
    pub fn validate_data(&self) -> Result<()> {
        self.payload.validate_data()?;
        ensure!(
            self.format_version == 1 && self.references == self.payload.references()?,
            "partial checkpoint strong edges differ from its exact payload"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_DOCUMENT_BYTES,
            "partial checkpoint envelope exceeds64KiB"
        );
        Ok(())
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let value: Self = decode(bytes, MAX_DOCUMENT_BYTES)?;
        value.validate_data()?;
        Ok(value)
    }
}

/// Producer-owned wrapper DATA retention only. Keep this JSON opaque in JS:
/// wrapper generations may exceed JavaScript's exact integer range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialCheckpointDataReceipt {
    pub format_version: u32,
    pub publication_id: String,
    pub publication: ObjectRef,
    pub wrappers: ObjectSetReceipt,
}
impl PartialCheckpointDataReceipt {
    pub fn validate_data(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported partial checkpoint data receipt"
        );
        publication_id(&self.publication_id)?;
        kind(
            &self.publication,
            ObjectKind::ActionResult,
            MAX_DOCUMENT_BYTES as u64,
        )?;
        validate_receipt(&self.wrappers)?;
        ensure!(
            self.wrappers.object_count == 4
                && self.wrappers.pin_id == producer_pin(&self.publication_id),
            "partial checkpoint wrapper receipt differs"
        );
        Ok(())
    }
    pub fn from_json(json: &str) -> Result<Self> {
        ensure!(
            json.len() <= MAX_DOCUMENT_BYTES,
            "partial checkpoint receipt exceeds64KiB"
        );
        let value: Self = serde_json::from_str(json)?;
        value.validate_data()?;
        Ok(value)
    }
}

/// DATA snapshot only, with no retention/grant constructor. TransferSession
/// validates it under its own prior staging and ACKs a separate exact clone.
pub struct PartialCheckpointData {
    publication: PartialCheckpointPublicationV1,
    q: QuiescedPartialCheckpointV1,
    objects: Vec<ObjectRef>,
}
impl PartialCheckpointData {
    pub fn publication(&self) -> &PartialCheckpointPublicationV1 {
        &self.publication
    }
    pub fn q(&self) -> &QuiescedPartialCheckpointV1 {
        &self.q
    }
    pub fn objects(&self) -> &[ObjectRef] {
        &self.objects
    }
}

/// Bounded prepared bytes. Not Serde and not physical authority. The caller
/// persists publication_id/ref before retain/network effects and owns staging.
pub struct PreparedPartialCheckpointData {
    store_root: PathBuf,
    staging_id: String,
    publication: PartialCheckpointPublicationV1,
    q: QuiescedPartialCheckpointV1,
    units: Vec<(ObjectRef, Vec<u8>)>,
}
impl PreparedPartialCheckpointData {
    pub fn publication(&self) -> &PartialCheckpointPublicationV1 {
        &self.publication
    }
    pub fn publication_reference(&self) -> Result<ObjectRef> {
        reference(ObjectKind::ActionResult, &self.publication)
    }
    /// Retain only Q/Source/Runnable/envelope wrapper bytes. Capture/evidence
    /// stay separately producer-owned. Errors never release uncertain effects.
    pub fn retain(&self, store: &LocalStore) -> Result<PartialCheckpointDataReceipt> {
        ensure!(
            store.root() == self.store_root,
            "prepared data belongs to another store"
        );
        ensure!(
            self.reconcile(store)?.is_none(),
            "producer ACK already exists; reconcile CURRENT same-operation DATA explicitly"
        );
        store.renew_staging(&self.staging_id, STAGING_TTL)?;
        let meter = Meter::new();
        inspect_q(store, &self.q, &self.publication.payload.template, &meter)?;
        for (reference, bytes) in &self.units {
            meter.check()?;
            store.receive_staged(&self.staging_id, reference, bytes)?;
        }
        let refs: Vec<_> = self
            .units
            .iter()
            .map(|(reference, _)| reference.clone())
            .collect();
        // The original stage is consumed by ACK. Hold a distinct read fence
        // continuously across that commit and every post-ACK producer read.
        recovery::with_read_staging(store, || {
            let receipt = store.retain_object_set(
                &self.staging_id,
                &producer_pin(&self.publication.payload.publication_id),
                &refs,
            )?;
            let result = PartialCheckpointDataReceipt {
                format_version: 1,
                publication_id: self.publication.payload.publication_id.clone(),
                publication: self.publication_reference()?,
                wrappers: receipt,
            };
            read_partial_checkpoint_data(store, &result)?;
            Ok(result)
        })
    }
    /// Observe CURRENT same-operation DATA using the original staging identity.
    /// Unknown generation never reconstructs a first ACK/cleanup capability.
    /// A replacement pin with another stage is never adopted or released.
    pub fn reconcile(
        &self,
        store: &LocalStore,
    ) -> Result<Option<CurrentPartialCheckpointDataRetention>> {
        ensure!(
            store.root() == self.store_root,
            "prepared data belongs to another store"
        );
        recovery::with_read_staging(store, || self.read_current_retention(store))
    }
    fn read_current_retention(
        &self,
        store: &LocalStore,
    ) -> Result<Option<CurrentPartialCheckpointDataRetention>> {
        let Some(wrappers) =
            store.object_set_receipt(&producer_pin(&self.publication.payload.publication_id))?
        else {
            return Ok(None);
        };
        let refs: Vec<_> = self
            .units
            .iter()
            .map(|(reference, _)| reference.clone())
            .collect();
        check_receipt_set(&wrappers, &self.staging_id, &refs)?;
        let receipt = PartialCheckpointDataReceipt {
            format_version: 1,
            publication_id: self.publication.payload.publication_id.clone(),
            publication: self.publication_reference()?,
            wrappers,
        };
        read_partial_checkpoint_data(store, &receipt)?;
        Ok(Some(CurrentPartialCheckpointDataRetention { receipt }))
    }
}

/// Dedicated live caller-owned staging must precede every producer CAS read.
/// This step writes no new wrapper objects; persist the returned immutable
/// identity before retain(). No private seed or inherited payload is read.
pub fn prepare_partial_checkpoint_data(
    store: &LocalStore,
    staging_id: &str,
    q: &QuiescedPartialCheckpointV1,
    template: &PartialCheckpointTemplateV1,
) -> Result<PreparedPartialCheckpointData> {
    store.renew_staging(staging_id, STAGING_TTL)?;
    prepare_from_data(store, staging_id, q, template)
}

// Caller holds actual GC staging. Recovery uses a distinct lease and must not
// renew/revive the old operation UUID merely to reconstruct its bounded bytes.
fn prepare_from_data(
    store: &LocalStore,
    staging_id: &str,
    q: &QuiescedPartialCheckpointV1,
    template: &PartialCheckpointTemplateV1,
) -> Result<PreparedPartialCheckpointData> {
    let meter = Meter::new();
    let (plan, _) = inspect_q(store, q, template, &meter)?;
    let source = SourceV2 {
        format_version: 2,
        consistency: CheckpointConsistency::Quiesced,
        directory: plan.directory.clone(),
        projection_policy: template.selection.projection_policy.clone(),
    };
    let source_ref = reference(ObjectKind::SourceRoot, &source)?;
    let runnable = RunnableRoot {
        format_version: 1,
        consistency: CheckpointConsistency::Quiesced,
        source: source_ref.clone(),
        environment: None,
        view_root: plan.directory,
        profile: template.selection.observation.profile.clone(),
        hardlinks: plan.hardlinks,
        knowledge_cursor: template.base_runnable.knowledge_cursor,
    };
    let runnable_ref = reference(ObjectKind::RunnableRoot, &runnable)?;
    let q_bytes = canonical_json(q)?;
    ensure!(
        q_bytes.len() <= MAX_Q_BYTES,
        "partial checkpoint Q exceeds32KiB"
    );
    let q_ref = ObjectRef::from_bytes(ObjectKind::Blob, &q_bytes);
    let mut payload = PartialCheckpointPayloadV1 {
        format_version: 1,
        kind: PURPOSE.into(),
        source_role: SOURCE_ROLE.into(),
        publication_id: String::new(),
        template: template.clone(),
        q: q_ref.clone(),
        source: source_ref.clone(),
        runnable: runnable_ref.clone(),
        plan: q.capture.retention.plan.clone(),
        freeze_intent: q.freeze_intent.clone(),
        drain_record: q.drain_record.clone(),
    };
    payload.publication_id = payload.expected_publication_id()?;
    let publication = PartialCheckpointPublicationV1 {
        format_version: 1,
        references: payload.references()?,
        payload,
    };
    publication.validate_data()?;
    let bytes = canonical_json(&publication)?;
    let units = vec![
        (q_ref, q_bytes),
        (source_ref, canonical_json(&source)?),
        (runnable_ref, canonical_json(&runnable)?),
        (
            ObjectRef::from_bytes(ObjectKind::ActionResult, &bytes),
            bytes,
        ),
    ];
    ensure!(
        bounded_objects(&units.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>())?.len() == 4,
        "partial checkpoint wrapper objects overlap"
    );
    meter.check()?;
    Ok(PreparedPartialCheckpointData {
        store_root: store.root().into(),
        staging_id: staging_id.into(),
        publication,
        q: q.clone(),
        units,
    })
}

/// Read-only DATA validation, not a grant. A transfer caller MUST start its own
/// staging before this call: producer pins may retire immediately afterward.
pub fn read_partial_checkpoint_data(
    store: &LocalStore,
    receipt: &PartialCheckpointDataReceipt,
) -> Result<PartialCheckpointData> {
    receipt.validate_data()?;
    let meter = Meter::new();
    let wrappers = store.object_set_refs(&receipt.wrappers)?;
    ensure!(
        wrappers.contains(&receipt.publication),
        "publication lacks exact wrapper retention"
    );
    let publication = PartialCheckpointPublicationV1::from_bytes(&read(
        store,
        &receipt.publication,
        MAX_DOCUMENT_BYTES,
    )?)?;
    ensure!(
        publication.payload.publication_id == receipt.publication_id,
        "publication receipt id differs"
    );
    let p = &publication.payload;
    let expected = bounded_objects(&[
        p.q.clone(),
        p.source.clone(),
        p.runnable.clone(),
        receipt.publication.clone(),
    ])?;
    ensure!(
        wrappers == expected && wrappers.len() == 4,
        "wrapper receipt retains another object set"
    );
    let q: QuiescedPartialCheckpointV1 = decode(&read(store, &p.q, MAX_Q_BYTES)?, MAX_Q_BYTES)?;
    let (plan, mut objects) = inspect_q(store, &q, &p.template, &meter)?;
    ensure!(
        p.plan == q.capture.retention.plan
            && p.freeze_intent == q.freeze_intent
            && p.drain_record == q.drain_record,
        "publication differs from its Q/evidence"
    );
    let source = snapshot::decode_source(&read(store, &p.source, snapshot::MAX_MANIFEST_BYTES)?)?;
    let runnable =
        snapshot::decode_runnable(&read(store, &p.runnable, snapshot::MAX_MANIFEST_BYTES)?)?;
    ensure!(
        source.directory == plan.directory
            && source.projection_policy == p.template.selection.projection_policy
            && runnable.source == p.source
            && runnable.view_root == plan.directory
            && runnable.environment.is_none()
            && runnable.profile == q.observation.profile
            && runnable.hardlinks == plan.hardlinks
            && runnable.knowledge_cursor == p.template.base_runnable.knowledge_cursor,
        "published source/runnable differs from the exact source-complement DATA"
    );
    objects.extend(wrappers);
    let objects = bounded_objects(&objects)?;
    meter.check()?;
    Ok(PartialCheckpointData {
        publication,
        q,
        objects,
    })
}

fn inspect_q(
    store: &LocalStore,
    q: &QuiescedPartialCheckpointV1,
    template: &PartialCheckpointTemplateV1,
    meter: &Meter,
) -> Result<(DeltaPlan, Vec<ObjectRef>)> {
    meter.check()?;
    q.validate_data()?;
    template.validate_data()?;
    ensure!(
        canonical_json(q)?.len() <= MAX_Q_BYTES,
        "partial checkpoint Q exceeds32KiB"
    );
    let selection = &template.selection;
    ensure!(
        q.capture.base == selection.root
            && q.capture.attempt == selection.attempt
            && q.observation == selection.observation,
        "Q differs from its source template"
    );
    let plan = delta::read_partial_tree(store, &q.capture.retention)?;
    kind(&plan.directory, ObjectKind::Directory, 4 * 1024 * 1024)?;
    ensure!(
        plan.format_version == 1
            && plan.capture_id == q.stop.request_id
            && plan.base == q.capture.base
            && plan.attempt == q.capture.attempt
            && plan.hardlinks == empty_topology()?
            && plan.local_objects.len() <= 16_000
            && plan.remote_boundaries.len() <= 4096,
        "partial checkpoint plan differs from its exact Q/base/topology"
    );
    let mut objects = store.object_set_refs(&q.capture.retention.local)?;
    ensure!(
        bounded_objects(&plan.local_objects)? == plan.local_objects,
        "partial checkpoint local provenance must be sorted and unique"
    );
    let mut previous_boundary = None;
    for path in &plan.remote_boundaries {
        transfer_reference(&path.reference)?;
        ensure!(
            path.edges.len() <= 1024
                && previous_boundary.is_none_or(|previous| previous < &path.reference)
                && plan.local_objects.binary_search(&path.reference).is_err(),
            "partial checkpoint boundaries exceed limits or overlap/repeat provenance"
        );
        previous_boundary = Some(&path.reference);
    }
    for reference in plan
        .local_objects
        .iter()
        .chain(plan.remote_boundaries.iter().map(|path| &path.reference))
    {
        ensure!(
            matches!(
                reference.kind,
                ObjectKind::Blob
                    | ObjectKind::File
                    | ObjectKind::Directory
                    | ObjectKind::DirectoryShard
            ),
            "partial checkpoint tree contains a non-content provenance ref"
        );
    }
    for reference in [&plan.directory, &plan.hardlinks] {
        ensure!(
            plan.local_objects.binary_search(reference).is_ok()
                || plan
                    .remote_boundaries
                    .iter()
                    .any(|path| &path.reference == reference),
            "partial checkpoint root/topology lacks explicit provenance"
        );
    }
    let evidence = bounded_objects(&[q.freeze_intent.clone(), q.drain_record.clone()])?;
    ensure!(
        evidence.len() == 2 && store.object_set_refs(&q.evidence_retention)? == evidence,
        "partial checkpoint evidence lacks exact two-Blob retention"
    );
    let freeze: RemoteFreezeIntent = decode(
        &read(store, &q.freeze_intent, MAX_FREEZE_BYTES)?,
        MAX_FREEZE_BYTES,
    )?;
    let launch_digest = digest_bytes(&canonical_json(selection)?);
    ensure!(
        freeze.format_version == 1
            && freeze.request.format_version == 1
            && freeze.launch_digest == launch_digest
            && freeze.request.capture.nonce == q.stop.request_id
            && freeze.request.capture.expected_begin == q.capture.begin
            && freeze.request.capture.expected_head == q.capture.previous_head
            && freeze.request.intent_id != q.stop.request_id,
        "freeze audit differs from Q/template"
    );
    canonical_uuid(&freeze.request.intent_id)?;
    let drain: DrainAuditDataV1 = decode(
        &read(store, &q.drain_record, MAX_DRAIN_BYTES)?,
        MAX_DRAIN_BYTES,
    )?;
    drain.validate_data(q, selection, &launch_digest)?;
    objects.extend(evidence);
    let objects = bounded_objects(&objects)?;
    meter.check()?;
    Ok((plan, objects))
}

/// Strict historical audit shape, NOT namespace/process authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceAuditIdentity {
    pub device: u64,
    pub inode: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkAuditDataV1 {
    pub format_version: u32,
    pub profile_digest: String,
    pub network_id: String,
    pub readiness_nonce: String,
    pub user: NamespaceAuditIdentity,
    pub network: NamespaceAuditIdentity,
    pub mode: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrainAuditDataV1 {
    pub format_version: u32,
    pub execution_id: String,
    pub operation_id: String,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub execution_request_digest: String,
    pub launch_request_digest: String,
    pub agent_exit: i32,
    pub stop: StopAndCheckpoint,
    pub partial_binding_digest: String,
    pub endpoint_instance: String,
    pub network: NetworkAuditDataV1,
    pub kernel_drained: bool,
}
impl DrainAuditDataV1 {
    fn validate_data(
        &self,
        q: &QuiescedPartialCheckpointV1,
        selection: &RemoteLaunchRequest,
        launch_digest: &str,
    ) -> Result<()> {
        let binding = PartialBinding {
            protocol: PROTOCOL.into(),
            format_version: 1,
            launch_request_digest: launch_digest.into(),
            root: selection.root.clone(),
            attempt: selection.attempt.clone(),
            observation: selection.observation.clone(),
        };
        canonical_uuid(&self.endpoint_instance)?;
        canonical_uuid(&self.network.network_id)?;
        canonical_uuid(&self.network.readiness_nonce)?;
        digest(&self.network.profile_digest)?;
        ensure!(
            self.format_version == 1
                && self.execution_id == q.stop.execution_id
                && self.operation_id == q.stop.operation_id
                && self.view_id == q.stop.view_id
                && self.owner == q.stop.owner
                && self.execution_request_digest == q.execution_request_digest
                && self.launch_request_digest == launch_digest
                && self.stop == q.stop
                && self.partial_binding_digest == binding.digest()?
                && self.kernel_drained
                && self.network.format_version == 1
                && self.network.mode == "offline_v1",
            "drain audit differs from Q/template/offline data profile"
        );
        Ok(())
    }
}

pub(crate) fn bounded_objects(objects: &[ObjectRef]) -> Result<Vec<ObjectRef>> {
    ensure!(
        objects.len() <= MAX_OBJECTS,
        "partial checkpoint inventory exceeds16384 objects"
    );
    let refs: Vec<_> = objects
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut total = 0u64;
    for reference in &refs {
        transfer_reference(reference)?;
        total = total
            .checked_add(reference.size_bytes)
            .context("partial checkpoint byte count overflow")?;
        ensure!(
            total <= MAX_BYTES,
            "partial checkpoint inventory exceeds4GiB"
        );
    }
    Ok(refs)
}
pub(crate) fn transfer_reference(reference: &ObjectRef) -> Result<()> {
    reference.validate()?;
    ensure!(
        reference.size_bytes <= 256 * 1024 * 1024
            && (reference.kind == ObjectKind::Blob || reference.size_bytes <= 4 * 1024 * 1024)
            && (!matches!(
                reference.kind,
                ObjectKind::File | ObjectKind::DirectoryShard | ObjectKind::RunnableRoot
            ) || reference.size_bytes <= 64 * 1024),
        "partial checkpoint object exceeds transfer format limits"
    );
    Ok(())
}
pub(crate) fn check_receipt_set(
    receipt: &ObjectSetReceipt,
    stage: &str,
    refs: &[ObjectRef],
) -> Result<()> {
    validate_receipt(receipt)?;
    let refs = bounded_objects(refs)?;
    let total = refs
        .iter()
        .try_fold(0u64, |total, r| total.checked_add(r.size_bytes))
        .context("retention byte count overflow")?;
    ensure!(
        receipt.staging_id == stage
            && receipt.object_count == refs.len() as u64
            && receipt.total_bytes == total
            && receipt.set_digest == digest_bytes(&canonical_json(&refs)?),
        "partial checkpoint clone/producer ACK differs from exact staging/object set"
    );
    Ok(())
}
fn validate_receipt(receipt: &ObjectSetReceipt) -> Result<()> {
    canonical_uuid(&receipt.staging_id)?;
    digest(&receipt.set_digest)?;
    ensure!(
        receipt.format_version == 1
            && receipt.generation > 0
            && receipt.generation <= i64::MAX as u64
            && receipt.object_count > 0
            && receipt.object_count <= MAX_OBJECTS as u64
            && receipt.total_bytes <= MAX_BYTES
            && !receipt.pin_id.is_empty()
            && receipt.pin_id.len() <= 512
            && !receipt.pin_id.chars().any(char::is_control),
        "invalid partial checkpoint retention receipt"
    );
    Ok(())
}
fn producer_pin(id: &str) -> String {
    format!("partial-checkpoint:producer:{id}")
}
fn publication_id(id: &str) -> Result<()> {
    digest(
        id.strip_prefix("pcd1-")
            .context("invalid partial checkpoint publication id")?,
    )
}
fn kind(reference: &ObjectRef, expected: ObjectKind, limit: u64) -> Result<()> {
    transfer_reference(reference)?;
    ensure!(
        reference.kind == expected && reference.size_bytes <= limit,
        "incorrect/oversized partial checkpoint reference"
    );
    Ok(())
}
fn reference(kind: ObjectKind, value: &impl Serialize) -> Result<ObjectRef> {
    Ok(ObjectRef::from_bytes(kind, &canonical_json(value)?))
}
fn empty_topology() -> Result<ObjectRef> {
    reference(
        ObjectKind::Blob,
        &HardlinkTopology {
            format_version: 1,
            groups: vec![],
        },
    )
}
fn digest_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid partial checkpoint digest"
    );
    Ok(())
}
fn canonical_uuid(value: &str) -> Result<()> {
    ensure!(
        Uuid::parse_str(value)?.to_string() == value,
        "noncanonical partial checkpoint UUID"
    );
    Ok(())
}
fn read(store: &LocalStore, reference: &ObjectRef, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        reference.size_bytes <= limit as u64,
        "partial checkpoint document exceeds its bound"
    );
    store.read_object(reference)
}
fn decode<T: DeserializeOwned + Serialize>(bytes: &[u8], limit: usize) -> Result<T> {
    ensure!(
        bytes.len() <= limit,
        "partial checkpoint document exceeds its bound"
    );
    let value: T = serde_json::from_slice(bytes)?;
    ensure!(
        canonical_json(&value)? == bytes,
        "partial checkpoint document must be canonical JSON"
    );
    Ok(value)
}
struct Meter(Instant);
impl Meter {
    fn new() -> Self {
        Self(Instant::now())
    }
    fn check(&self) -> Result<()> {
        ensure!(
            self.0.elapsed() <= VALIDATION_TIME,
            "partial checkpoint data validation exceeded cooperative deadline"
        );
        Ok(())
    }
}
