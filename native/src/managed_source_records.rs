//! Off-App immutable source-assignment and publication recovery DATA only.
//! These records never reconstruct a grant, producer ACK, active retention,
//! shutdown join or permission to launch. Unknown ACKs require reconciliation.
use super::*;
use crate::managed_launch::RemoteSourceAssignment;
use rudder_engine::{
    execution::QuiescedPartialCheckpointV1,
    partial_checkpoint::{
        PartialCheckpointDataIntent, PartialCheckpointDataReceipt,
        PartialCheckpointPublicationV1, PartialCheckpointTemplateV1, PreparedPartialCheckpointData,
        MAX_Q_BYTES,
    },
    protocol::{ObjectKind, ObjectRef},
};

// The bundle includes the original Q, template and intent, plus the bounded
// publication envelope needed to recheck their immutable identity without CAS.
const SOURCE_RECORD_LIMIT: usize = 256 * 1024;
const INPUTS_FILE: &str = "checkpoint-data-inputs.json";
const RECEIPT_FILE: &str = "checkpoint-data-receipt.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceCheckpointInputs {
    pub format_version: u32,
    pub fence: ManagedLaunchFence,
    pub q: QuiescedPartialCheckpointV1,
    pub template: PartialCheckpointTemplateV1,
    pub intent: PartialCheckpointDataIntent,
    pub publication: PartialCheckpointPublicationV1,
}
impl SourceCheckpointInputs {
    /// Call after DATA preparation but BEFORE retain/network effects. The
    /// prepared object provides its exact publication and original staging ID.
    pub(crate) fn from_prepared(
        fence: ManagedLaunchFence,
        q: QuiescedPartialCheckpointV1,
        template: PartialCheckpointTemplateV1,
        prepared: &PreparedPartialCheckpointData,
    ) -> Result<Self> {
        let value = Self { format_version: 1, fence, q, template,
            intent: prepared.intent()?, publication: prepared.publication().clone() };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<()> {
        self.fence.validate()?;
        self.q.validate_data()?;
        self.template.validate_data()?;
        self.intent.validate_data()?;
        self.publication.validate_data()?;
        let selection = &self.template.selection;
        let q = &self.q;
        let payload = &self.publication.payload;
        ensure!(self.format_version == 1 && canonical_json(q)?.len() <= MAX_Q_BYTES,
            "source checkpoint input version/size differs");
        ensure!(selection.request_id == self.fence.request_id
            && selection.attempt.project_id == self.fence.owner.project_id
            && selection.attempt.run_id == self.fence.run_id
            && selection.attempt.run_epoch == self.fence.run_epoch
            && selection.attempt.execution_id == self.fence.attempt_id
            && selection.attempt.owner == self.fence.owner_epoch(),
            "source checkpoint selection differs from its reserved fence");
        ensure!(q.capture.base == selection.root && q.capture.attempt == selection.attempt
            && q.observation == selection.observation && payload.template == self.template
            && payload.q == ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(q)?)
            && payload.plan == q.capture.retention.plan && payload.freeze_intent == q.freeze_intent
            && payload.drain_record == q.drain_record
            && self.intent.publication_id == payload.publication_id
            && self.intent.publication == ObjectRef::from_bytes(ObjectKind::ActionResult,
                &canonical_json(&self.publication)?),
            "source checkpoint original inputs/publication differ");
        ensure!(canonical_json(self)?.len() <= SOURCE_RECORD_LIMIT,
            "source checkpoint inputs exceed bound");
        Ok(())
    }
    fn validate_receipt(&self, json: &str) -> Result<()> {
        let receipt = PartialCheckpointDataReceipt::from_json(json)?;
        let p = &self.publication.payload;
        let mut refs = vec![p.q.clone(), p.source.clone(), p.runnable.clone(),
            self.intent.publication.clone()];
        refs.sort();
        refs.dedup();
        let total = refs.iter().try_fold(0u64, |sum, r| sum.checked_add(r.size_bytes)
            .context("source wrapper byte count overflow"))?;
        ensure!(receipt.publication_id == self.intent.publication_id
            && receipt.publication == self.intent.publication
            && receipt.wrappers.staging_id == self.intent.staging_id && refs.len() == 4
            && receipt.wrappers.object_count == refs.len() as u64
            && receipt.wrappers.total_bytes == total
            && receipt.wrappers.set_digest == hex_digest(&canonical_json(&refs)?),
            "source producer receipt differs from original input intent");
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceCheckpointReceiptData {
    format_version: u32,
    data_only: bool,
    fence: ManagedLaunchFence,
    // The original opaque string is persisted verbatim, including u64 tokens
    // and intentional whitespace. It is never replaced by reserialized JSON.
    receipt_json: String,
}
fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}
fn read_source_record<T: DeserializeOwned + Serialize>(path: &Path) -> Result<Option<T>> {
    let Some(bytes) = read_bytes(path, SOURCE_RECORD_LIMIT)? else { return Ok(None) };
    let value = serde_json::from_slice::<T>(&bytes)?;
    ensure!(canonical_json(&value)? == bytes, "noncanonical source recovery record");
    Ok(Some(value))
}
fn immutable_source<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = canonical_json(value)?;
    ensure!(bytes.len() <= SOURCE_RECORD_LIMIT, "source recovery record exceeds bound");
    let parent = path.parent().context("source record parent absent")?;
    private_directory(parent)?;
    match OpenOptions::new().create_new(true).write(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path) {
        Ok(mut file) => { file.write_all(&bytes)?; file.sync_all()?; }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(read_bytes(path, SOURCE_RECORD_LIMIT)?.as_deref() == Some(bytes.as_slice()),
                "immutable source evidence differs; explicit recovery required");
            OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn refuse_legacy_assignment(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory.join("assignment.json")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => anyhow::bail!("source attempt collides with local workspace assignment"),
    }
}
impl ManagedRunLocation {
    fn check_source_assignment(&self, assignment: &RemoteSourceAssignment) -> Result<PathBuf> {
        self.check_fence(&assignment.fence)?;
        assignment.validate(&assignment.fence)?;
        let reservation = self.reservation(&assignment.fence.attempt_id)?
            .context("source assignment has no durable reservation")?;
        let ManagedRunSource::Reserved { fence, runnable_root } = reservation else {
            anyhow::bail!("source assignment requires its original reservation")
        };
        ensure!(assignment.fence == fence && assignment.selection.root.root == runnable_root,
            "source assignment differs from reserved fence/root");
        let directory = self.attempt_directory(&fence.attempt_id)?;
        refuse_legacy_assignment(&directory)?;
        Ok(directory)
    }
    pub(crate) fn persist_source_assignment(&self, assignment: &RemoteSourceAssignment) -> Result<()> {
        let directory = self.check_source_assignment(assignment)?;
        ensure!(canonical_json(assignment)?.len() <= RECORD_LIMIT, "source assignment exceeds bound");
        immutable_source(&directory.join("assignment2.json"), assignment)
    }
    pub(crate) fn source_assignment(&self, attempt: &str) -> Result<Option<RemoteSourceAssignment>> {
        self.reservation(attempt)?.context("source reservation missing")?;
        let directory = self.attempt_directory(attempt)?;
        refuse_legacy_assignment(&directory)?;
        let assignment: Option<RemoteSourceAssignment> = read_record(&directory.join("assignment2.json"))?;
        if let Some(value) = &assignment {
            ensure!(value.fence.attempt_id == attempt, "source assignment attempt differs");
            self.check_source_assignment(value)?;
        }
        Ok(assignment)
    }
    fn check_source_inputs(&self, inputs: &SourceCheckpointInputs) -> Result<PathBuf> {
        inputs.validate()?;
        let assignment = self.source_assignment(&inputs.fence.attempt_id)?
            .context("source checkpoint assignment missing")?;
        ensure!(inputs.fence == assignment.fence && inputs.template.selection == assignment.selection,
            "source checkpoint differs from exact reserved source assignment");
        self.attempt_directory(&inputs.fence.attempt_id)
    }
    /// Return only after file and parent fsync, before producer retain/upload.
    pub(crate) fn persist_source_checkpoint_inputs(&self, inputs: &SourceCheckpointInputs) -> Result<()> {
        let directory = self.check_source_inputs(inputs)?;
        immutable_source(&directory.join(INPUTS_FILE), inputs)
    }
    /// Original recovery DATA only. Receipt presence or absence cannot recover
    /// an original ACK; callers use explicit engine reconciliation as needed.
    pub(crate) fn source_checkpoint_inputs(&self, attempt: &str) -> Result<Option<SourceCheckpointInputs>> {
        self.source_assignment(attempt)?.context("source checkpoint assignment missing")?;
        let inputs: Option<SourceCheckpointInputs> = read_source_record(&self.attempt_directory(attempt)?.join(INPUTS_FILE))?;
        if let Some(value) = &inputs {
            ensure!(value.fence.attempt_id == attempt, "source checkpoint attempt differs");
            self.check_source_inputs(value)?;
        }
        Ok(inputs)
    }
    /// Accepts an actual reply OR reconciled-current opaque DATA. Persistence
    /// proves neither the original ACK nor current retention/cleanup authority.
    pub(crate) fn persist_source_checkpoint_receipt(&self, fence: &ManagedLaunchFence, receipt_json: &str) -> Result<()> {
        self.check_fence(fence)?;
        let inputs = self.source_checkpoint_inputs(&fence.attempt_id)?
            .context("source checkpoint inputs must precede producer receipt")?;
        ensure!(inputs.fence == *fence, "source producer receipt fence differs");
        inputs.validate_receipt(receipt_json)?;
        immutable_source(&self.attempt_directory(&fence.attempt_id)?.join(RECEIPT_FILE),
            &SourceCheckpointReceiptData { format_version: 1, data_only: true, fence: fence.clone(), receipt_json: receipt_json.into() })
    }
    /// Returns the exact saved DATA string, never a recovered ACK capability.
    pub(crate) fn source_checkpoint_receipt(&self, attempt: &str) -> Result<Option<String>> {
        let inputs = self.source_checkpoint_inputs(attempt)?.context("source checkpoint inputs missing")?;
        let receipt: Option<SourceCheckpointReceiptData> = read_source_record(&self.attempt_directory(attempt)?.join(RECEIPT_FILE))?;
        receipt.map(|value| {
            ensure!(value.format_version == 1 && value.data_only && value.fence == inputs.fence,
                "source producer receipt identity differs");
            inputs.validate_receipt(&value.receipt_json)?;
            Ok(value.receipt_json)
        }).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_store::tests::Fixture;
    use rudder_engine::execution::ProtectedHostContextRef;

    #[derive(Deserialize)]
    struct Data { q_json: String, template_json: String, envelope_json: String, receipt_json: String }
    struct SourceFixture { _owner: Fixture, location: ManagedRunLocation, assignment: RemoteSourceAssignment,
        inputs: SourceCheckpointInputs, receipt_json: String }
    fn rebind_publication(inputs: &mut SourceCheckpointInputs) {
        inputs.publication.payload.template = inputs.template.clone();
        inputs.publication.payload.q = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&inputs.q).unwrap());
        inputs.publication.payload.publication_id = inputs.publication.payload.expected_publication_id().unwrap();
        inputs.publication.references = inputs.publication.payload.references().unwrap();
        inputs.intent.publication_id = inputs.publication.payload.publication_id.clone();
        inputs.intent.publication = ObjectRef::from_bytes(ObjectKind::ActionResult, &canonical_json(&inputs.publication).unwrap());
    }
    fn fixture() -> SourceFixture {
        let data: Data = serde_json::from_str(include_str!("../../tests/fixtures/partial-checkpoint-v1.json")).unwrap();
        let mut owner = Fixture::new(); let store = owner.prepare();
        let mut template: PartialCheckpointTemplateV1 = serde_json::from_str(&data.template_json).unwrap();
        let mut q: QuiescedPartialCheckpointV1 = serde_json::from_str(&data.q_json).unwrap();
        let mut publication: PartialCheckpointPublicationV1 = serde_json::from_str(&data.envelope_json).unwrap();
        let mut receipt = PartialCheckpointDataReceipt::from_json(&data.receipt_json).unwrap();
        let fence = ManagedLaunchFence { owner: owner.owner.clone(), run_id: template.selection.attempt.run_id.clone(),
            run_epoch: 1, attempt_id: template.selection.attempt.execution_id.clone(), request_id: template.selection.request_id.clone() };
        template.selection.attempt.project_id = fence.owner.project_id.clone();
        template.selection.attempt.owner = fence.owner_epoch(); template.selection.attempt.run_epoch = fence.run_epoch;
        template.selection.observation.owner = fence.owner_epoch();
        q.capture.attempt = template.selection.attempt.clone(); q.capture.owner = fence.owner_epoch();
        q.capture.begin.owner = fence.owner_epoch(); q.capture.end.owner = fence.owner_epoch();
        q.observation = template.selection.observation.clone(); q.stop.owner = fence.owner_epoch(); q.stop.run_epoch = fence.run_epoch;
        publication.payload.template = template.clone();
        publication.payload.q = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&q).unwrap());
        publication.payload.publication_id = publication.payload.expected_publication_id().unwrap();
        publication.references = publication.payload.references().unwrap();
        let intent = PartialCheckpointDataIntent { format_version: 1, staging_id: receipt.wrappers.staging_id.clone(),
            publication_id: publication.payload.publication_id.clone(),
            publication: ObjectRef::from_bytes(ObjectKind::ActionResult, &canonical_json(&publication).unwrap()) };
        let inputs = SourceCheckpointInputs { format_version: 1, fence: fence.clone(), q, template, intent, publication };
        receipt.publication_id = inputs.intent.publication_id.clone(); receipt.publication = inputs.intent.publication.clone();
        receipt.wrappers.pin_id = format!("partial-checkpoint:producer:{}", receipt.publication_id);
        let p = &inputs.publication.payload;
        let mut refs = vec![p.q.clone(), p.source.clone(), p.runnable.clone(), inputs.intent.publication.clone()]; refs.sort();
        receipt.wrappers.set_digest = hex_digest(&canonical_json(&refs).unwrap());
        receipt.wrappers.total_bytes = refs.iter().map(|r|r.size_bytes).sum();
        let assignment = RemoteSourceAssignment { format_version: 2, fence: fence.clone(), selection: inputs.template.selection.clone(),
            host_context: ProtectedHostContextRef { format_version: 1, path: "/private/context.json".into(), sha256: "b".repeat(64), size_bytes: 1 } };
        let location = store.run(&fence.run_id).unwrap();
        location.persist_reservation(&ManagedRunSource::Reserved { fence, runnable_root: assignment.selection.root.root.clone() }).unwrap();
        SourceFixture { _owner: owner, location, assignment, inputs, receipt_json: serde_json::to_string(&receipt).unwrap() }
    }
    #[test]
    fn source_assignment_replays_exactly_and_rejects_wrong_fence_base_view_and_local_collision() {
        let f = fixture(); f.location.persist_source_assignment(&f.assignment).unwrap();
        f.location.persist_source_assignment(&f.assignment).unwrap();
        assert_eq!(f.location.source_assignment(&f.assignment.fence.attempt_id).unwrap(), Some(f.assignment.clone()));
        for change in 0..4 {
            let mut wrong = f.assignment.clone();
            match change { 0 => wrong.fence.request_id = uuid::Uuid::new_v4().to_string(),
                1 => wrong.selection.root.root = ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"other"),
                2 => wrong.selection.observation.view_id = uuid::Uuid::new_v4().to_string(),
                _ => wrong.host_context.sha256 = "c".repeat(64) }
            assert!(f.location.persist_source_assignment(&wrong).is_err());
        }
        let directory = f.location.attempt_directory(&f.assignment.fence.attempt_id).unwrap();
        fs::write(directory.join("assignment.json"), b"foreign family").unwrap();
        assert!(f.location.source_assignment(&f.assignment.fence.attempt_id).is_err());
        assert!(f.location.persist_source_assignment(&f.assignment).is_err());
    }
    #[test]
    fn source_checkpoint_persists_exact_recovery_inputs_before_ack_and_preserves_opaque_u64_receipt() {
        let f = fixture(); let attempt = &f.assignment.fence.attempt_id;
        f.location.persist_source_assignment(&f.assignment).unwrap();
        assert!(f.location.persist_source_checkpoint_receipt(&f.assignment.fence, &f.receipt_json).is_err());
        f.location.persist_source_checkpoint_inputs(&f.inputs).unwrap();
        f.location.persist_source_checkpoint_inputs(&f.inputs).unwrap();
        assert!(f.location.source_checkpoint_receipt(attempt).unwrap().is_none());
        let restored = f.location.source_checkpoint_inputs(attempt).unwrap().unwrap();
        assert_eq!(canonical_json(&restored).unwrap(), canonical_json(&f.inputs).unwrap());
        let opaque = format!(" \n{}\n", f.receipt_json);
        f.location.persist_source_checkpoint_receipt(&f.assignment.fence, &opaque).unwrap();
        f.location.persist_source_checkpoint_receipt(&f.assignment.fence, &opaque).unwrap();
        assert_eq!(f.location.source_checkpoint_receipt(attempt).unwrap(), Some(opaque));
        assert!(f.location.persist_source_checkpoint_receipt(&f.assignment.fence, &f.receipt_json).is_err());
        assert!(PartialCheckpointDataReceipt::from_json(&f.receipt_json).unwrap().wrappers.generation > (1 << 53));
    }
    #[test]
    fn source_checkpoint_rejects_wrong_assignment_attempt_intent_and_conflicting_immutable_inputs() {
        let f = fixture(); f.location.persist_source_assignment(&f.assignment).unwrap();
        for change in 0..4 {
            let mut wrong = f.inputs.clone();
            match change { 0 => wrong.fence.run_epoch += 1,
                1 => wrong.q.capture.attempt.operation_id = uuid::Uuid::new_v4().to_string(),
                2 => wrong.template.selection.root.root_id = "foreign-base".into(),
                _ => wrong.intent.publication = ObjectRef::from_bytes(ObjectKind::ActionResult, b"foreign") }
            assert!(f.location.persist_source_checkpoint_inputs(&wrong).is_err());
        }
        // Internally valid foreign base/operation/view DATA must still fail
        // the exact durable assignment check, not merely Q self-validation.
        for change in 0..3 {
            let mut wrong = f.inputs.clone();
            if change == 0 {
                wrong.template.selection.root.root_id = "foreign-base".into();
                wrong.q.capture.base = wrong.template.selection.root.clone();
            } else if change == 1 {
                let id = uuid::Uuid::new_v4().to_string();
                wrong.template.selection.attempt.operation_id = id.clone();
                wrong.template.selection.observation.attempt.operation_id = id.clone();
                wrong.q.capture.attempt.operation_id = id.clone();
                wrong.q.stop.operation_id = id;
                wrong.q.observation = wrong.template.selection.observation.clone();
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                wrong.template.selection.attempt.view_id = id.clone();
                wrong.template.selection.observation.view_id = id.clone();
                wrong.q.capture.attempt.view_id = id.clone();
                wrong.q.capture.view_id = id.clone();
                wrong.q.capture.begin.view_id = id.clone(); wrong.q.capture.end.view_id = id.clone();
                wrong.q.stop.view_id = id;
                wrong.q.observation = wrong.template.selection.observation.clone();
            }
            rebind_publication(&mut wrong);
            wrong.validate().unwrap();
            assert!(f.location.persist_source_checkpoint_inputs(&wrong).is_err());
        }
        f.location.persist_source_checkpoint_inputs(&f.inputs).unwrap();
        let mut conflicting = f.inputs.clone(); conflicting.intent.staging_id = uuid::Uuid::new_v4().to_string();
        assert!(f.location.persist_source_checkpoint_inputs(&conflicting).is_err());
        let mut receipt = PartialCheckpointDataReceipt::from_json(&f.receipt_json).unwrap();
        receipt.wrappers.staging_id = uuid::Uuid::new_v4().to_string();
        assert!(f.location.persist_source_checkpoint_receipt(&f.assignment.fence, &serde_json::to_string(&receipt).unwrap()).is_err());
    }
    #[test]
    fn source_records_preserve_documents_larger_than_legacy_limit_and_refuse_oversize() {
        let f = fixture(); let directory = f.location.attempt_directory(&f.assignment.fence.attempt_id).unwrap();
        let path = directory.join("bounded-source-data.json"); let value = "x".repeat(RECORD_LIMIT + 64);
        immutable_source(&path, &value).unwrap();
        assert_eq!(read_source_record::<String>(&path).unwrap(), Some(value));
        let oversized = directory.join("oversized-source-data.json");
        assert!(immutable_source(&oversized, &"x".repeat(SOURCE_RECORD_LIMIT)).is_err());
        assert!(!oversized.exists());
    }
}
