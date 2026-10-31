//! Local receipt authority, independent of the project journal and CAS pins.
use super::record::{
    decode_envelope, encode_envelope, CommittedExecutionRecord, ExecutionIdentityV2,
    ExecutionRecordV1, ExecutionRecordV2, ExecutionStateV2,
};
use super::workspace::{
    RemoteEvidenceLease, RemoteFinalizationRecovery, RemoteFinalizationReservation,
};
use super::*;
use crate::{
    protocol::{ObjectKind, ObjectRef},
    store::{LocalStore, ObjectSetReceipt},
    vfs::{FrozenRemoteView, PartialCaptureBudget, PartialCaptureReceipt, VfsView},
};
use anyhow::{ensure, Context};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::Duration,
};

const REMOTE_RESERVATION_FILE: &str = "remote-finalization.json";
const REMOTE_LEASE_FILE: &str = "remote-evidence-lease.json";
const REMOTE_RECEIPT_FILE: &str = "remote-evidence-receipt.json";
const MAX_REMOTE_EVIDENCE_FILE: u64 = 64 * 1024;
const REMOTE_EVIDENCE_TTL: Duration = Duration::from_secs(600);

pub(crate) struct State {
    pub directory: PathBuf,
    record: ExecutionRecord,
    is_new: bool,
    manager_root: PathBuf,
    store_root: PathBuf,
    workspace_attempted: bool,
    workspace_opened: bool,
    finalization_attempted: bool,
    expects_remote: bool,
    remote_commit: Option<RemoteCommitResources>,
    lock_identity: LockIdentity,
    _lock: File,
}
struct RemoteCommitResources {
    // Actual already-open resources; verification must never reopen a saved
    // manager/store path or turn historical data into live authority.
    view: VfsView,
    store: LocalStore,
    reservation: RemoteFinalizationReservation,
    lease: RemoteEvidenceLease,
    receipt: ObjectSetReceipt,
    capture: PartialCaptureReceipt,
}
/// Both variants keep the actual lock. Historical data never invokes a new
/// service/workspace factory or restores the old process/session authority.
pub(crate) enum Preflight {
    Recovered(State),
    New {
        state: State,
        services: Option<HelperServices>,
    },
}
pub(crate) fn preflight(
    spec: &ExecutionSpec,
    factory: Option<HelperServicesFactory>,
) -> Result<Preflight> {
    let mut state = State::open(spec)?;
    if !state.is_new {
        state
            .record
            .checkpointed_data()
            .context("execution has uncertain prior effects; reconcile its orphan before retry")?;
        state.check_lock()?;
        return Ok(Preflight::Recovered(state));
    }
    let services = (|| -> Result<Option<HelperServices>> {
        if spec.workspace_observation.is_some() {
            Ok(Some(factory.context(
                "workspace helper lacks a qualified authority factory",
            )?(spec)?))
        } else {
            Ok(None)
        }
    })();
    match services {
        Ok(services) => {
            state.check_lock()?;
            Ok(Preflight::New { state, services })
        }
        Err(error) => {
            let _ = state.mark_orphaned();
            Err(error)
        }
    }
}
/// Private new-reservation evidence. It never establishes liveness or an
/// attempt grant; admission also requires the actual held lock and authority.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockIdentity {
    format_version: u32,
    execution_id: String,
    operation_id: String,
    view_id: String,
    owner: OwnerEpoch,
    request_digest: String,
    directory_device: u64,
    directory_inode: u64,
    device: u64,
    inode: u64,
    nonce: String,
}
impl State {
    pub fn open(spec: &ExecutionSpec) -> Result<Self> {
        spec.validate()?;
        // These are path identities only. No VFS manager, LocalStore, remote
        // process, view or authority factory is opened during preflight.
        let manager_root = canonical_directory_or_future(&spec.manager_root)?;
        let store_root = canonical_directory_or_future(&spec.store_root)?;
        let root = &spec.execution_root;
        ensure!(
            root.is_absolute() && root != Path::new("/"),
            "invalid private execution root"
        );
        let marker = root.join("execution-manager.json");
        if !root.try_exists()? {
            ensure!(
                root.parent().context("execution root parent")?.is_dir(),
                "execution root parent must already exist"
            );
            fs::create_dir(root)?;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
            File::open(root.parent().unwrap())?.sync_all()?;
        }
        let m = fs::symlink_metadata(root)?;
        ensure!(
            m.is_dir()
                && !m.file_type().is_symlink()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0,
            "execution root is not a private owned real directory"
        );
        let expected = crate::protocol::canonical_json(
            &serde_json::json!({"format_version":1,"manager_root":manager_root,"store_root":store_root}),
        )?;
        if let Some(bytes) = read_private(&marker, 64 * 1024)? {
            ensure!(bytes == expected, "execution manager binding differs");
        } else {
            ensure!(
                fs::read_dir(root)?.next().is_none(),
                "refusing nonempty non-execution directory"
            );
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&marker)?;
            f.write_all(&expected)?;
            f.sync_all()?;
            File::open(root)?.sync_all()?;
        }
        for name in ["runs"] {
            private_state_directory(&root.join(name))?;
        }
        let directory = root.join("runs").join(&spec.execution_id);
        private_state_directory(&directory)?;
        let digest = hex::encode(Sha256::digest(crate::protocol::canonical_json(spec)?));
        let mut lock_options = OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let lock_path = directory.join("owner.lock");
        let (mut lock, new_lock) = match fs::symlink_metadata(&lock_path) {
            Ok(_) => (lock_options.open(&lock_path)?, false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    fs::read_dir(&directory)?.next().is_none(),
                    "execution lock is missing beside retained evidence; explicit recovery required"
                );
                (lock_options.create_new(true).open(&lock_path)?, true)
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = lock.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "unsafe execution owner lock"
        );
        lock.try_lock_exclusive()
            .context("execution already has a live supervisor")?;
        let directory_metadata = fs::symlink_metadata(&directory)?;
        let identity_path = directory.join("lock-identity.json");
        let lock_identity = if new_lock {
            let nonce = Uuid::new_v4().to_string();
            lock.write_all(nonce.as_bytes())?;
            lock.sync_all()?;
            let identity = LockIdentity {
                format_version: 1,
                execution_id: spec.execution_id.clone(),
                operation_id: spec.operation_id.clone(),
                view_id: spec.view_id.clone(),
                owner: spec.owner.clone(),
                request_digest: digest.clone(),
                directory_device: directory_metadata.dev(),
                directory_inode: directory_metadata.ino(),
                device: metadata.dev(),
                inode: metadata.ino(),
                nonce,
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&identity_path)?;
            file.write_all(&crate::protocol::canonical_json(&identity)?)?;
            file.sync_all()?;
            File::open(&directory)?.sync_all()?;
            identity
        } else {
            let bytes = read_private(&identity_path, 16 * 1024)?.context(
                "execution lacks lock-identity evidence; original data is historical recovery-only",
            )?;
            let identity: LockIdentity = serde_json::from_slice(&bytes)?;
            ensure!(
                identity.format_version == 1
                    && identity.execution_id == spec.execution_id
                    && identity.operation_id == spec.operation_id
                    && identity.view_id == spec.view_id
                    && identity.owner == spec.owner
                    && identity.request_digest == digest,
                "execution lock identity differs from exact request; explicit recovery required"
            );
            identity
        };
        check_lock_identity(&directory, &lock, &lock_identity)?;
        let path = directory.join("record.json");
        let persisted = read_private(&path, 1024 * 1024)?;
        ensure!(
            persisted.is_some() || new_lock,
            "execution record is missing after a prior reservation; explicit reconciliation required"
        );
        let is_new = persisted.is_none();
        let record = if let Some(bytes) = persisted {
            let r = decode_envelope(&bytes)?;
            validate_spec_record(spec, &digest, &r)?;
            r
        } else {
            initial_record(spec, digest)?
        };
        let state = Self {
            directory,
            record,
            is_new,
            manager_root,
            store_root,
            workspace_attempted: false,
            workspace_opened: false,
            finalization_attempted: false,
            expects_remote: spec.profile.as_ref().is_some_and(|profile| {
                matches!(
                    &profile.workspace,
                    ExecutionWorkspaceSelectionV4::RemoteSourceOnly { .. }
                )
            }),
            remote_commit: None,
            lock_identity,
            _lock: lock,
        };
        // Validate retained evidence before invoking any services or workspace
        // factory. Recovery never turns these snapshots back into live guards.
        read_evidence_recovery(root, &state.record)?;
        if state.is_new {
            state.persist_record(&state.record)?;
        }
        Ok(state)
    }
    fn new_execution<'a>(&'a self, spec: &'a ExecutionSpec) -> Result<NewExecution<'a>> {
        self.check_lock()?;
        ensure!(
            self.is_new
                && self.record.phase() == ExecutionPhase::Preparing
                && self.record.request_digest()
                    == hex::encode(Sha256::digest(crate::protocol::canonical_json(spec)?)),
            "only the exact newly locked execution may construct a workspace"
        );
        Ok(NewExecution::locked(
            spec,
            &self.directory,
            &self.manager_root,
            &self.store_root,
            self.record.request_digest(),
            &self._lock,
        ))
    }
    pub(crate) fn open_workspace(
        &mut self,
        spec: &ExecutionSpec,
        factory: Option<HelperWorkspaceFactory>,
    ) -> Result<OwnedHelperWorkspace> {
        ensure!(
            !self.workspace_attempted,
            "workspace factory already attempted for this execution"
        );
        self.new_execution(spec)?;
        self.workspace_attempted = true;
        let request = self.new_execution(spec)?;
        let workspace = match factory {
            Some(factory) => factory(&request),
            None => OwnedHelperWorkspace::local(&request),
        }?;
        self.check_lock()?;
        ensure!(
            spec.profile.is_none()
                || self.expects_remote
                    == matches!(
                        workspace.checkpoint_capability(),
                        CheckpointCapability::PartialRemoteUnavailable
                            | CheckpointCapability::PartialRemoteFinalizable
                    ),
            "workspace factory changed the sealed local/remote selection"
        );
        self.workspace_opened = true;
        Ok(workspace)
    }
    pub(crate) fn record(&self) -> &ExecutionRecord {
        &self.record
    }
    #[cfg(test)]
    pub(crate) fn is_new(&self) -> bool {
        self.is_new
    }
    pub(crate) fn into_historical(self) -> HistoricalExecution {
        HistoricalExecution::from_record(self.record)
    }
    pub(crate) fn mark_running(&mut self) -> Result<()> {
        self.check_lock()?;
        ensure!(
            self.is_new && self.workspace_opened,
            "only a new opened execution may run"
        );
        let mut record = self.record.clone();
        record.set_running()?;
        self.persist_record(&record)?;
        self.record = record;
        Ok(())
    }
    pub(crate) fn mark_orphaned(&mut self) -> Result<()> {
        self.check_lock()?;
        ensure!(
            self.is_new,
            "historical execution cannot become a new orphan transition"
        );
        let mut record = self.record.clone();
        record.set_orphaned()?;
        self.persist_record(&record)?;
        self.record = record;
        Ok(())
    }
    pub(crate) fn checkpoint_workspace(
        &self,
        workspace: &OwnedHelperWorkspace,
    ) -> Result<VfsCheckpoint> {
        // A named-lock/directory/sidecar change retires this execution even
        // after the workspace opened. Check before CAS/staging/retention effects;
        // the post-check withholds publication if custody changes mid-capture.
        self.check_lock()?;
        ensure!(
            self.is_new
                && self.workspace_opened
                && self.record.phase() != ExecutionPhase::Checkpointed,
            "checkpoint data requires the newly opened execution"
        );
        let descriptor = workspace.view().descriptor()?;
        ensure!(
            descriptor.view_id == self.record.view_id()
                && descriptor.owner == *self.record.owner()
                && workspace.manager().root().canonicalize()? == self.manager_root
                && workspace.manager().store().root().canonicalize()? == self.store_root,
            "checkpoint workspace differs from the held execution"
        );
        ensure!(
            !self.expects_remote,
            "remote checkpoint requires actual partial finalization"
        );
        workspace.require_supported_execution()?;
        let checkpoint = workspace.view().checkpoint_snapshot_unmounted()?;
        self.check_lock()?;
        Ok(checkpoint)
    }
    /// Commits local checkpoint DATA only. The trusted Linux caller separately
    /// owns its actual PID/VCS/RPC/FUSE drain before a shutdown acknowledgment.
    /// No remote artifact or deserialized physical-proof token is accepted.
    pub(crate) fn finalize_local(
        &mut self,
        workspace: &OwnedHelperWorkspace,
        agent_exit: i32,
    ) -> Result<CommittedExecutionRecord> {
        self.check_lock()?;
        ensure!(
            self.is_new
                && self.workspace_opened
                && !self.finalization_attempted
                && self.record.phase() == ExecutionPhase::Running
                && !self.expects_remote,
            "execution cannot retry or change local finalization"
        );
        self.finalization_attempted = true;
        let checkpoint = self.checkpoint_workspace(workspace)?;
        let mut record = self.record.clone();
        record.set_complete_local(checkpoint, agent_exit)?;
        self.persist_record(&record)?;
        let committed = CommittedExecutionRecord::new(record.clone())?;
        self.record = record;
        Ok(committed)
    }
    /// The only remote terminal transition. Serialized audit data cannot call
    /// this boundary without the actual Linux drain and actual frozen owner.
    /// All failures retain freeze/capture/evidence effects for explicit recovery.
    pub(crate) fn finalize_remote(
        &mut self,
        workspace: &OwnedHelperWorkspace,
        drain: &super::linux::OwnedExecutionDrain,
        frozen: &mut FrozenRemoteView,
        budget: PartialCaptureBudget,
    ) -> Result<CommittedExecutionRecord> {
        self.check_lock()?;
        ensure!(
            self.is_new
                && self.workspace_opened
                && !self.finalization_attempted
                && self.expects_remote
                && self.record.phase() == ExecutionPhase::Running,
            "execution cannot retry or change remote finalization"
        );
        let running = self.record.clone();
        self.check_remote_workspace(workspace, frozen)?;
        let audit = drain.validate_remote_finalization(&running, workspace)?;
        ensure!(audit.len() <= 32 * 1024, "remote drain audit exceeds bound");
        let stop = drain.authenticated_stop()?.clone();
        ensure!(
            frozen.intent().request.capture.nonce == stop.request_id,
            "strong capture requires the actual fresh authenticated stop nonce"
        );
        ensure!(
            workspace
                .view()
                .inspect_partial_capture(&stop.request_id)?
                .is_none(),
            "pre-existing live capture cannot become a quiescent checkpoint"
        );
        let ExecutionRecord::V2(record) = &running else {
            anyhow::bail!("remote finalization requires record2")
        };
        let reservation = RemoteFinalizationReservation {
            format_version: 1,
            identity: record.identity.clone(),
            freeze: frozen.intent().clone(),
            freeze_reference: frozen.intent_reference(),
            drain_record: ObjectRef::from_bytes(ObjectKind::Blob, &audit),
            stop,
            agent_exit: drain.agent_exit(),
            evidence_pin: format!(
                "execution:evidence:{}:{}",
                self.record.execution_id(),
                frozen.intent().request.intent_id
            ),
        };
        reservation.validate_data()?;
        self.check_remote_guard(workspace, drain, frozen, &running, &audit)?;
        self.finalization_attempted = true;
        self.reserve_remote_evidence(&reservation)?;
        // A crash between minting this lease and saving it leaves only a TTL
        // lease. No evidence bytes or capture effects precede the saved lease.
        let store = workspace.manager().store();
        let lease = self.begin_remote_evidence_lease(store, &reservation)?;
        let check = || self.check_remote_guard(workspace, drain, frozen, &running, &audit);
        let evidence_retention = self.retain_remote_evidence(
            store,
            &reservation,
            &lease,
            frozen.intent_bytes(),
            &audit,
            &check,
        )?;
        check()?;
        // Public capture admissions are closed by the held freeze. Recheck
        // absence immediately before the sole capture call; never reconcile an
        // older LiveBestEffort ACK into this stronger result or recapture errors.
        ensure!(
            workspace
                .view()
                .inspect_partial_capture(&reservation.stop.request_id)?
                .is_none(),
            "capture nonce appeared before quiescent capture"
        );
        // Capture checks actual RemoteSession authority itself. Its cancellation
        // callback must not re-enter the held view DB via frozen/drain getters.
        let capture = frozen.capture(&budget, &|| self.check_lock().is_err())?;
        self.check_remote_guard(workspace, drain, frozen, &running, &audit)?;
        self.check_remote_capture(workspace, frozen, &capture)?;
        self.check_remote_evidence(store, &reservation, &lease, &evidence_retention)?;
        let checkpoint = QuiescedPartialCheckpointV1 {
            format_version: 1,
            capture,
            stop: reservation.stop.clone(),
            observation: workspace
                .remote_request()
                .context("remote request disappeared")?
                .observation
                .clone(),
            execution_request_digest: running.request_digest().into(),
            freeze_intent: reservation.freeze_reference.clone(),
            drain_record: reservation.drain_record.clone(),
            evidence_retention,
        };
        let mut record = running.clone();
        record.set_partial_remote(checkpoint, drain.agent_exit())?;
        let committed = CommittedExecutionRecord::new(record.clone())?;
        drain.validate_commit(&committed)?;
        self.check_remote_guard(workspace, drain, frozen, &running, &audit)?;
        let Some(CheckpointRef::PartialRemote(checkpoint)) = record.checkpoint() else {
            unreachable!()
        };
        self.check_remote_capture(workspace, frozen, &checkpoint.capture)?;
        self.check_remote_evidence(store, &reservation, &lease, &checkpoint.evidence_retention)?;
        self.persist_record(&record)?;
        self.remote_commit = Some(RemoteCommitResources {
            view: workspace.view().clone(),
            store: store.clone(),
            reservation: reservation.clone(),
            lease: lease.clone(),
            receipt: checkpoint.evidence_retention.clone(),
            capture: checkpoint.capture.clone(),
        });
        self.record = record;
        // Durable bytes may exist after a late fence/error, but no current
        // committed result is returned without these final authority checks.
        self.check_remote_guard(workspace, drain, frozen, &running, &audit)?;
        let Some(CheckpointRef::PartialRemote(checkpoint)) = committed.record().checkpoint() else {
            unreachable!()
        };
        self.check_remote_capture(workspace, frozen, &checkpoint.capture)?;
        self.check_remote_evidence(store, &reservation, &lease, &checkpoint.evidence_retention)?;
        drain.validate_commit(&committed)?;
        self.verify_commit_current(&committed)?;
        Ok(committed)
    }
    fn check_remote_workspace(
        &self,
        workspace: &OwnedHelperWorkspace,
        frozen: &FrozenRemoteView,
    ) -> Result<()> {
        self.check_lock()?;
        ensure!(
            self.expects_remote
                && workspace.checkpoint_capability()
                    == CheckpointCapability::PartialRemoteFinalizable,
            "remote finalization requires the sealed namespace5 capability"
        );
        let descriptor = workspace.view().descriptor()?;
        let remote = workspace
            .remote_request()
            .context("remote finalization lacks source binding")?;
        let ExecutionRecord::V2(record) = &self.record else {
            anyhow::bail!("remote finalization requires record2")
        };
        ensure!(
            workspace.manager().root().canonicalize()? == self.manager_root
                && workspace.manager().store().root().canonicalize()? == self.store_root
                && descriptor.view_id == record.identity.view_id
                && descriptor.owner == record.identity.owner
                && descriptor.remote_finalization_format == Some(1)
                && remote.attempt.project_id == record.identity.project_id
                && remote.request_id == record.identity.request_id
                && remote.attempt.run_id == record.identity.run_id
                && remote.attempt.run_epoch == record.identity.run_epoch
                && remote.attempt.execution_id == record.identity.execution_id
                && remote.attempt.operation_id == record.identity.operation_id
                && remote.attempt.view_id == record.identity.view_id
                && remote.attempt.owner == record.identity.owner
                && workspace.manager().remote_root_binding() == Some(&remote.root)
                && frozen.intent().launch_digest
                    == hex::encode(Sha256::digest(crate::protocol::canonical_json(remote)?)),
            "remote finalization workspace differs from the held execution"
        );
        frozen.check_for(workspace.view())?;
        self.check_lock()
    }
    fn check_remote_guard(
        &self,
        workspace: &OwnedHelperWorkspace,
        drain: &super::linux::OwnedExecutionDrain,
        frozen: &FrozenRemoteView,
        running: &ExecutionRecord,
        audit: &[u8],
    ) -> Result<()> {
        self.check_remote_workspace(workspace, frozen)?;
        ensure!(
            drain.validate_remote_finalization(running, workspace)? == audit,
            "actual drain evidence changed"
        );
        self.check_lock()
    }
    fn check_remote_capture(
        &self,
        workspace: &OwnedHelperWorkspace,
        frozen: &FrozenRemoteView,
        capture: &PartialCaptureReceipt,
    ) -> Result<()> {
        frozen.check_for(workspace.view())?;
        let remote = workspace
            .remote_request()
            .context("remote request disappeared")?;
        let request = &frozen.intent().request.capture;
        ensure!(
            capture.request_nonce == request.nonce
                && capture.previous_head == request.expected_head
                && capture.begin == request.expected_begin
                && capture.end == request.expected_begin
                && capture.attempt == remote.attempt
                && capture.base == remote.root
                && capture.exclusion_policy_digest == remote.observation.exclusion_policy_digest,
            "frozen capture differs from the exact source/intent"
        );
        Self::check_capture_ack(workspace.view(), capture)?;
        self.check_lock()
    }
    fn check_capture_ack(view: &VfsView, capture: &PartialCaptureReceipt) -> Result<()> {
        let remote = view.remote_launch_request()?;
        ensure!(
            capture.base == remote.root
                && capture.attempt == remote.attempt
                && capture.view_id == remote.observation.view_id
                && capture.owner == remote.observation.owner
                && capture.begin.namespace == remote.observation.namespace
                && capture.exclusion_policy_digest == remote.observation.exclusion_policy_digest,
            "current capture differs from the actual source owner"
        );
        let status = view
            .inspect_partial_capture(&capture.request_nonce)?
            .context("frozen capture missing")?;
        ensure!(
            status.state == "committed"
                && status.current
                && status.receipt.as_ref() == Some(capture)
                && status.retention.as_ref() == Some(&capture.retention)
                && view.partial_capture_head()?.as_ref() == Some(capture),
            "frozen capture lacks its exact current data ACK"
        );
        crate::delta::read_partial_tree(view.manager().store(), &capture.retention)?;
        Ok(())
    }
    fn reserve_remote_evidence(&self, reservation: &RemoteFinalizationReservation) -> Result<()> {
        reservation.validate_data()?;
        self.check_lock()?;
        ensure!(
            matches!(&self.record, ExecutionRecord::V2(record) if record.identity == reservation.identity)
                && self.record.phase() == ExecutionPhase::Running
                && self.expects_remote,
            "remote evidence requires the exact running record2"
        );
        for name in [
            REMOTE_RESERVATION_FILE,
            REMOTE_LEASE_FILE,
            REMOTE_RECEIPT_FILE,
        ] {
            ensure!(
                fs::symlink_metadata(self.directory.join(name))
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "remote finalization evidence already exists or is unsafe; recovery required"
            );
        }
        self.write_immutable_evidence(REMOTE_RESERVATION_FILE, reservation)
    }
    fn begin_remote_evidence_lease(
        &self,
        store: &LocalStore,
        reservation: &RemoteFinalizationReservation,
    ) -> Result<RemoteEvidenceLease> {
        ensure!(
            store.root().canonicalize()? == self.store_root,
            "evidence store differs from the held execution"
        );
        self.check_evidence_file(REMOTE_RESERVATION_FILE, reservation)?;
        self.check_lock()?;
        let staging = store.begin_staging(REMOTE_EVIDENCE_TTL)?;
        let lease = RemoteEvidenceLease {
            format_version: 1,
            reservation_digest: reservation.digest()?,
            staging,
        };
        self.write_immutable_evidence(REMOTE_LEASE_FILE, &lease)?;
        Ok(lease)
    }
    /// DATA retention only, independently testable without manufacturing a
    /// process-drain guard. The caller supplies its real pre/post checks.
    fn retain_remote_evidence(
        &self,
        store: &LocalStore,
        reservation: &RemoteFinalizationReservation,
        lease: &RemoteEvidenceLease,
        freeze: &[u8],
        audit: &[u8],
        check: &dyn Fn() -> Result<()>,
    ) -> Result<ObjectSetReceipt> {
        ensure!(
            store.root().canonicalize()? == self.store_root,
            "evidence store differs from the held execution"
        );
        reservation.validate_data()?;
        ensure!(
            ObjectRef::from_bytes(ObjectKind::Blob, freeze) == reservation.freeze_reference
                && ObjectRef::from_bytes(ObjectKind::Blob, audit) == reservation.drain_record,
            "evidence bytes differ from the durable reservation"
        );
        self.check_evidence_file(REMOTE_RESERVATION_FILE, reservation)?;
        self.check_evidence_file(REMOTE_LEASE_FILE, lease)?;
        validate_evidence_lease(reservation, lease)?;
        check()?;
        self.check_lock()?;
        store.receive_staged(&lease.staging.id, &reservation.freeze_reference, freeze)?;
        check()?;
        self.check_lock()?;
        store.receive_staged(&lease.staging.id, &reservation.drain_record, audit)?;
        check()?;
        self.check_lock()?;
        let receipt = store.retain_object_set(
            &lease.staging.id,
            &reservation.evidence_pin,
            &reservation.references(),
        )?;
        // Even a failed final post-check leaves the acknowledged object set and
        // the discoverable pin/staging identity in place. Never undo uncertain effects.
        self.check_lock()?;
        self.write_immutable_evidence(REMOTE_RECEIPT_FILE, &receipt)?;
        check()?;
        self.check_remote_evidence(store, reservation, lease, &receipt)?;
        Ok(receipt)
    }
    fn check_remote_evidence(
        &self,
        store: &LocalStore,
        reservation: &RemoteFinalizationReservation,
        lease: &RemoteEvidenceLease,
        receipt: &ObjectSetReceipt,
    ) -> Result<()> {
        self.check_lock()?;
        self.check_evidence_file(REMOTE_RESERVATION_FILE, reservation)?;
        self.check_evidence_file(REMOTE_LEASE_FILE, lease)?;
        self.check_evidence_file(REMOTE_RECEIPT_FILE, receipt)?;
        validate_evidence_receipt(reservation, lease, receipt)?;
        ensure!(
            store.object_set_refs(receipt)? == reservation.references(),
            "evidence pin differs from its exact two Blobs"
        );
        for reference in reservation.references() {
            let bytes = store.read_object(&reference)?;
            ensure!(
                ObjectRef::from_bytes(ObjectKind::Blob, &bytes) == reference,
                "retained evidence Blob changed"
            );
        }
        self.check_lock()
    }
    fn write_immutable_evidence(&self, name: &str, value: &impl Serialize) -> Result<()> {
        self.check_lock()?;
        let bytes = crate::protocol::canonical_json(value)?;
        ensure!(
            bytes.len() as u64 <= MAX_REMOTE_EVIDENCE_FILE,
            "remote recovery evidence exceeds bound"
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.directory.join(name))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        File::open(&self.directory)?.sync_all()?;
        self.check_lock()?;
        self.check_evidence_file(name, value)
    }
    fn check_evidence_file(&self, name: &str, value: &impl Serialize) -> Result<()> {
        ensure!(
            read_private(&self.directory.join(name), MAX_REMOTE_EVIDENCE_FILE)?
                .is_some_and(|bytes| crate::protocol::canonical_json(value)
                    .is_ok_and(|expected| bytes == expected)),
            "durable remote recovery evidence changed or is missing"
        );
        Ok(())
    }
    fn persist_record(&self, record: &ExecutionRecord) -> Result<()> {
        self.check_lock()?;
        ensure!(
            record.format_version() == self.record.format_version()
                && record.execution_id() == self.record.execution_id()
                && record.operation_id() == self.record.operation_id()
                && record.view_id() == self.record.view_id()
                && record.owner() == self.record.owner()
                && record.request_digest() == self.record.request_digest()
                && record.supervisor_pid() == self.record.supervisor_pid(),
            "execution transition changed immutable identity"
        );
        if let (ExecutionRecord::V2(next), ExecutionRecord::V2(current)) = (record, &self.record) {
            ensure!(
                next.identity == current.identity,
                "execution2 transition changed immutable identity"
            );
        }
        let bytes = encode_envelope(record)?;
        let temp = self.directory.join(format!(".record-{}", Uuid::new_v4()));
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(temp, self.directory.join("record.json"))?;
        File::open(&self.directory)?.sync_all()?;
        self.check_lock()?;
        Ok(())
    }
    /// Recheck durable DATA immediately before the executor joins its actual
    /// drain guard and publishes control. This creates no physical authority.
    pub(crate) fn verify_commit_current(&self, committed: &CommittedExecutionRecord) -> Result<()> {
        self.check_lock()?;
        ensure!(
            self.is_new && self.finalization_attempted,
            "historical data cannot be a current State commit"
        );
        self.record.checkpointed_data()?;
        let expected = crate::protocol::canonical_json(&self.record)?;
        ensure!(
            crate::protocol::canonical_json(committed.record())? == expected
                && committed.checksum() == hex::encode(Sha256::digest(&expected)),
            "committed execution data differs from the held State"
        );
        let bytes = read_private(
            &self.directory.join("record.json"),
            super::record::MAX_EXECUTION_RECORD_BYTES as u64,
        )?
        .context("committed execution record is missing")?;
        let persisted = decode_envelope(&bytes)?;
        ensure!(
            crate::protocol::canonical_json(&persisted)? == expected,
            "committed execution record changed before publication"
        );
        if let Some(CheckpointRef::PartialRemote(checkpoint)) = self.record.checkpoint() {
            let retained = self
                .remote_commit
                .as_ref()
                .context("partial commit lacks its actual retained resources")?;
            ensure!(
                retained.capture == checkpoint.capture
                    && retained.receipt == checkpoint.evidence_retention
                    && retained.reservation.freeze_reference == checkpoint.freeze_intent
                    && retained.reservation.drain_record == checkpoint.drain_record,
                "partial commit differs from its retained resources"
            );
            let launch = retained.view.remote_launch_request()?;
            ensure!(
                hex::encode(Sha256::digest(crate::protocol::canonical_json(launch)?))
                    == retained.reservation.freeze.launch_digest,
                "partial commit source binding changed before publication"
            );
            Self::check_capture_ack(&retained.view, &retained.capture)?;
            self.check_remote_evidence(
                &retained.store,
                &retained.reservation,
                &retained.lease,
                &retained.receipt,
            )?;
        }
        self.check_lock()
    }
    fn check_lock(&self) -> Result<()> {
        check_lock_identity(&self.directory, &self._lock, &self.lock_identity)
    }
}

fn initial_record(spec: &ExecutionSpec, request_digest: String) -> Result<ExecutionRecord> {
    let supervisor_pid = std::process::id();
    let record = match spec.format_version {
        1..=3 => ExecutionRecord::V1(ExecutionRecordV1 {
            format_version: 1,
            execution_id: spec.execution_id.clone(),
            operation_id: spec.operation_id.clone(),
            view_id: spec.view_id.clone(),
            owner: spec.owner.clone(),
            request_digest,
            phase: ExecutionPhase::Preparing,
            supervisor_pid,
            agent_exit: None,
            checkpoint: None,
        }),
        4 | 5 => {
            let profile = spec
                .profile
                .as_ref()
                .context("record2 requires a sealed profile")?;
            let broker = spec
                .broker_binding
                .as_ref()
                .context("record2 requires a scoped broker")?;
            ExecutionRecord::V2(ExecutionRecordV2 {
                format_version: 2,
                identity: ExecutionIdentityV2 {
                    project_id: broker.project_id.clone(),
                    request_id: profile.request_id.clone(),
                    run_id: broker.run_id.clone(),
                    run_epoch: broker.run_epoch,
                    execution_id: spec.execution_id.clone(),
                    operation_id: spec.operation_id.clone(),
                    view_id: spec.view_id.clone(),
                    owner: spec.owner.clone(),
                    request_digest,
                    supervisor_pid,
                },
                state: ExecutionStateV2::Preparing,
            })
        }
        _ => anyhow::bail!("unsupported execution record selection"),
    };
    record.validate_metadata()?;
    Ok(record)
}
fn validate_spec_record(
    spec: &ExecutionSpec,
    digest: &str,
    record: &ExecutionRecord,
) -> Result<()> {
    let expected_version = match spec.format_version {
        1..=3 => 1,
        4 | 5 => 2,
        _ => anyhow::bail!("unsupported execution record selection"),
    };
    ensure!(
        record.format_version() == expected_version
            && record.request_digest() == digest
            && record.owner() == &spec.owner
            && record.view_id() == spec.view_id
            && record.operation_id() == spec.operation_id
            && record.execution_id() == spec.execution_id,
        "execution id is bound to different inputs/owner/version"
    );
    if let ExecutionRecord::V2(record) = record {
        let profile = spec
            .profile
            .as_ref()
            .context("record2 requires a sealed profile")?;
        let broker = spec
            .broker_binding
            .as_ref()
            .context("record2 requires a scoped broker")?;
        ensure!(
            record.identity.request_id == profile.request_id
                && record.identity.project_id == broker.project_id
                && record.identity.run_id == broker.run_id
                && record.identity.run_epoch == broker.run_epoch,
            "execution2 identity differs from the sealed profile"
        );
    }
    Ok(())
}

fn private_state_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "unsafe private execution state directory; explicit recovery required"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            File::open(path)?.sync_all()?;
            File::open(path.parent().context("execution state directory parent")?)?.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn check_lock_identity(directory: &Path, lock: &File, identity: &LockIdentity) -> Result<()> {
    let held = lock.metadata()?;
    let named = fs::symlink_metadata(directory.join("owner.lock"))?;
    let directory_metadata = fs::symlink_metadata(directory)?;
    ensure!(
        directory_metadata.is_dir()
            && !directory_metadata.file_type().is_symlink()
            && directory_metadata.uid() == unsafe { libc::geteuid() }
            && directory_metadata.mode() & 0o077 == 0
            && directory_metadata.dev() == identity.directory_device
            && directory_metadata.ino() == identity.directory_inode,
        "execution directory identity changed; explicit recovery required"
    );
    for metadata in [&held, &named] {
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && metadata.nlink() == 1
                && metadata.dev() == identity.device
                && metadata.ino() == identity.inode
                && metadata.len() == 36,
            "execution lock identity changed; explicit recovery required"
        );
    }
    ensure!(
        Uuid::parse_str(&identity.nonce)?.to_string() == identity.nonce,
        "invalid execution lock nonce"
    );
    let mut nonce = [0u8; 36];
    std::os::unix::fs::FileExt::read_exact_at(lock, &mut nonce, 0)?;
    ensure!(
        nonce == identity.nonce.as_bytes(),
        "execution lock nonce differs"
    );
    let expected = crate::protocol::canonical_json(identity)?;
    ensure!(
        read_private(&directory.join("lock-identity.json"), 16 * 1024)?
            .is_some_and(|bytes| bytes == expected),
        "execution lock sidecar changed; explicit recovery required"
    );
    Ok(())
}

fn validate_evidence_lease(
    reservation: &RemoteFinalizationReservation,
    lease: &RemoteEvidenceLease,
) -> Result<()> {
    ensure!(
        lease.format_version == 1
            && lease.reservation_digest == reservation.digest()?
            && Uuid::parse_str(&lease.staging.id)?.to_string() == lease.staging.id
            && lease.staging.expires_at_ms > 0,
        "remote evidence lease differs from the immutable reservation"
    );
    Ok(())
}
fn validate_evidence_receipt(
    reservation: &RemoteFinalizationReservation,
    lease: &RemoteEvidenceLease,
    receipt: &ObjectSetReceipt,
) -> Result<()> {
    reservation.validate_data()?;
    validate_evidence_lease(reservation, lease)?;
    let refs = reservation.references();
    let total = reservation
        .freeze_reference
        .size_bytes
        .checked_add(reservation.drain_record.size_bytes)
        .context("remote evidence byte count overflow")?;
    ensure!(
        receipt.format_version == 1
            && receipt.pin_id == reservation.evidence_pin
            && receipt.staging_id == lease.staging.id
            && receipt.generation > 0
            && receipt.generation <= i64::MAX as u64
            && receipt.object_count == 2
            && receipt.total_bytes == total
            && receipt.set_digest
                == hex::encode(Sha256::digest(crate::protocol::canonical_json(&refs)?)),
        "remote evidence ACK differs from its exact reservation/lease/two Blobs"
    );
    Ok(())
}
fn read_canonical_evidence<T: serde::de::DeserializeOwned + Serialize>(
    directory: &Path,
    name: &str,
) -> Result<Option<T>> {
    read_private(&directory.join(name), MAX_REMOTE_EVIDENCE_FILE)?
        .map(|bytes| {
            let value: T = serde_json::from_slice(&bytes)?;
            ensure!(
                crate::protocol::canonical_json(&value)? == bytes,
                "noncanonical remote recovery evidence"
            );
            Ok(value)
        })
        .transpose()
}
/// Bounded historical DATA only. Never opens a CAS store, refreshes a lease,
/// checks current grants, imports evidence or reconstructs a frozen owner.
pub(super) fn read_evidence_recovery(
    root: &Path,
    record: &ExecutionRecord,
) -> Result<Option<RemoteFinalizationRecovery>> {
    let directory = root.join("runs").join(record.execution_id());
    let reservation: Option<RemoteFinalizationReservation> =
        read_canonical_evidence(&directory, REMOTE_RESERVATION_FILE)?;
    let lease: Option<RemoteEvidenceLease> =
        read_canonical_evidence(&directory, REMOTE_LEASE_FILE)?;
    let receipt: Option<ObjectSetReceipt> =
        read_canonical_evidence(&directory, REMOTE_RECEIPT_FILE)?;
    let Some(reservation) = reservation else {
        ensure!(
            lease.is_none()
                && receipt.is_none()
                && !matches!(record.checkpoint(), Some(CheckpointRef::PartialRemote(_))),
            "remote evidence reservation is missing; explicit recovery required"
        );
        return Ok(None);
    };
    reservation.validate_data()?;
    let ExecutionRecord::V2(current) = record else {
        anyhow::bail!("remote evidence exists beside legacy record")
    };
    ensure!(
        current.identity == reservation.identity,
        "remote recovery evidence belongs to another execution"
    );
    if let Some(lease) = &lease {
        validate_evidence_lease(&reservation, lease)?;
    }
    if let Some(receipt) = &receipt {
        validate_evidence_receipt(
            &reservation,
            lease
                .as_ref()
                .context("evidence ACK lacks its saved lease")?,
            receipt,
        )?;
    }
    if let Some(CheckpointRef::PartialRemote(checkpoint)) = record.checkpoint() {
        let capture = &reservation.freeze.request.capture;
        ensure!(
            checkpoint.freeze_intent == reservation.freeze_reference
                && checkpoint.drain_record == reservation.drain_record
                && checkpoint.stop == reservation.stop
                && checkpoint.capture.begin == capture.expected_begin
                && checkpoint.capture.end == capture.expected_begin
                && checkpoint.capture.previous_head == capture.expected_head
                && record.agent_exit() == Some(reservation.agent_exit)
                && receipt.as_ref() == Some(&checkpoint.evidence_retention),
            "terminal remote record differs from its durable evidence"
        );
    } else {
        ensure!(
            record.phase() != ExecutionPhase::Checkpointed,
            "complete local checkpoint has unexpected remote recovery evidence"
        );
    }
    Ok(Some(RemoteFinalizationRecovery {
        reservation,
        lease,
        receipt,
    }))
}

pub(super) fn read_recovery_record(
    root: &Path,
    execution_id: &str,
) -> Result<Option<ExecutionRecord>> {
    ensure!(
        root.is_absolute()
            && root != Path::new("/")
            && root.components().all(|part| matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )),
        "historical execution root requires a normalized absolute path"
    );
    ensure!(
        Uuid::parse_str(execution_id)?.to_string() == execution_id,
        "historical execution id must be a canonical UUID"
    );
    let runs = root.join("runs");
    let directory = runs.join(execution_id);
    for path in [root, runs.as_path(), directory.as_path()] {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "historical execution directory has unsafe type/owner/mode"
        );
    }
    let Some(bytes) = read_private(&directory.join("record.json"), 1024 * 1024)? else {
        for name in [
            REMOTE_RESERVATION_FILE,
            REMOTE_LEASE_FILE,
            REMOTE_RECEIPT_FILE,
        ] {
            ensure!(fs::symlink_metadata(directory.join(name)).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "remote finalization evidence lacks its execution record; explicit recovery required");
        }
        return Ok(None);
    };
    let record = decode_envelope(&bytes)?;
    ensure!(
        record.execution_id() == execution_id,
        "historical execution record identity differs"
    );
    Ok(Some(record))
}

fn read_private(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= limit,
        "unsafe or oversized private execution record"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "private execution record exceeds bound"
    );
    Ok(Some(bytes))
}
fn canonical_directory_or_future(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute()
            && path.components().all(|part| matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )),
        "execution workspace requires normalized absolute paths"
    );
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    missing.len() < 128,
                    "execution workspace path exceeds bounded depth"
                );
                missing.push(
                    existing
                        .file_name()
                        .context("workspace path has no existing ancestor")?
                        .to_os_string(),
                );
                ensure!(existing.pop(), "workspace path has no existing ancestor");
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut canonical = existing.canonicalize()?;
    ensure!(canonical.is_dir(), "workspace ancestor is not a directory");
    for part in missing.into_iter().rev() {
        canonical.push(part);
    }
    Ok(canonical)
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
