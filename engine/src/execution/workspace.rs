//! Trusted pre-mount workspace construction. These types contain actual owned
//! resources, never serialized permission or a saved-session constructor.
//!
//! New reservations persist a private lock-identity sidecar before any factory
//! effects. Legacy executions without it, missing/mismatched lock evidence, and
//! copies or backup restores that change device/inode identities are historical
//! recovery-only. Their original records/checkpoints/pins are not repaired or
//! deleted. V1-v3 specification and Record1 canonical bytes stay unchanged.
//! The sidecar is neither liveness nor a grant: admission needs the actual held
//! lock and current attempt authority. The read-only recovery API opens no
//! services, views, sessions or locks and never authorizes retry or resume.
use super::ExecutionSpec;
use crate::{
    protocol::{ObjectKind, ObjectRef},
    store::{LocalStore, ObjectSetReceipt, StagingLease},
    vfs::{
        remote_launch::{RemoteLaunchRequest, RemoteLaunchWorkspace},
        RemoteFreezeIntent, VfsManager, VfsView,
    },
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs::File, path::Path};

/// Borrowed proof that the executor holds the actual state lock for this new
/// request. Only execution preflight constructs it. It grants no broker/root
/// authority and cannot be restored, cloned or sent as a persisted receipt.
pub struct NewExecution<'a> {
    spec: &'a ExecutionSpec,
    directory: &'a Path,
    manager_root: &'a Path,
    store_root: &'a Path,
    digest: &'a str,
    _held_lock: &'a File,
}
impl<'a> NewExecution<'a> {
    pub(super) fn locked(
        spec: &'a ExecutionSpec,
        directory: &'a Path,
        manager_root: &'a Path,
        store_root: &'a Path,
        digest: &'a str,
        lock: &'a File,
    ) -> Self {
        Self {
            spec,
            directory,
            manager_root,
            store_root,
            digest,
            _held_lock: lock,
        }
    }
    pub fn spec(&self) -> &ExecutionSpec {
        self.spec
    }
    pub fn directory(&self) -> &Path {
        self.directory
    }
    pub fn manager_root(&self) -> &Path {
        self.manager_root
    }
    pub fn store_root(&self) -> &Path {
        self.store_root
    }
    pub fn request_digest(&self) -> &str {
        self.digest
    }
}

/// The services factory is itself constructed before namespace entry without
/// starting threads. This second factory runs only after namespace admission,
/// state reservation/reconciliation and current services authorization.
pub type HelperWorkspaceFactory =
    Box<dyn FnOnce(&NewExecution<'_>) -> Result<OwnedHelperWorkspace>>;

/// Concrete source/cache/registry/session/authority guards belong here. Drop
/// does not certify transport settlement, release an uncertain pin, or prove a
/// checkpoint. The supervisor retains this owner through finalization/fsync.
pub trait HelperWorkspaceResources: Send {}
impl<T: Send> HelperWorkspaceResources for T {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointCapability {
    CompleteLocal,
    /// Preparation only. No outer remote execution version or quiescent
    /// partial-finalization protocol has been allocated by this seam.
    PartialRemoteUnavailable,
    /// Requires actual drain, an owned namespace5 freeze, exact data/evidence
    /// retention and durable Record2. The capability itself proves none of them.
    PartialRemoteFinalizable,
}

/// Historical data only, including pre-sidecar records. This type carries no
/// held lock, source pin, process witness or current attempt grant.
pub struct HistoricalExecution {
    record: super::ExecutionRecord,
    remote_evidence: Option<RemoteFinalizationRecovery>,
}
impl HistoricalExecution {
    pub(super) fn from_record(record: super::ExecutionRecord) -> Self {
        // A supervisor's recovered outcome carries record DATA only. Bounded
        // recovery-file snapshots are loaded by read_execution_recovery; this
        // conversion does not reopen state or restore its old live resources.
        Self {
            record,
            remote_evidence: None,
        }
    }
    pub fn record(&self) -> &super::ExecutionRecord {
        &self.record
    }
    /// Snapshot of durable recovery DATA, including an unfinished finalization.
    /// This neither checks an active pin nor reconstructs any held guard.
    pub fn remote_evidence(&self) -> Option<&RemoteFinalizationRecovery> {
        self.remote_evidence.as_ref()
    }
}
/// Immutable recovery reservation written before staging/CAS/capture effects.
/// Public decoding permits audit only, never re-entry or finalization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFinalizationReservation {
    pub format_version: u32,
    pub identity: super::ExecutionIdentityV2,
    pub freeze: RemoteFreezeIntent,
    pub freeze_reference: ObjectRef,
    pub drain_record: ObjectRef,
    pub stop: super::StopAndCheckpoint,
    pub agent_exit: i32,
    pub evidence_pin: String,
}
impl RemoteFinalizationReservation {
    pub(super) fn validate_data(&self) -> Result<()> {
        self.identity.validate()?;
        let capture = &self.freeze.request.capture;
        for id in [
            &self.freeze.request.intent_id,
            &capture.nonce,
            &self.stop.request_id,
            &capture.expected_begin.namespace,
        ] {
            ensure!(
                uuid::Uuid::parse_str(id)?.to_string() == *id,
                "invalid evidence reservation identity"
            );
        }
        if let Some(head) = &capture.expected_head {
            ensure!(
                uuid::Uuid::parse_str(head)?.to_string() == *head,
                "invalid evidence reservation head"
            );
        }
        self.freeze_reference.validate()?;
        self.drain_record.validate()?;
        let expected = ObjectRef::from_bytes(
            ObjectKind::Blob,
            &crate::protocol::canonical_json(&self.freeze)?,
        );
        ensure!(
            self.format_version == 1
                && self.freeze.format_version == 1
                && self.freeze.request.format_version == 1
                && self.freeze_reference == expected
                && self.freeze_reference.size_bytes <= 64 * 1024
                && self.drain_record.kind == ObjectKind::Blob
                && self.drain_record.size_bytes <= 32 * 1024
                && self.drain_record != self.freeze_reference
                && self.freeze.launch_digest.len() == 64
                && self
                    .freeze
                    .launch_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && self.freeze.request.intent_id != capture.nonce
                && capture.nonce == self.stop.request_id
                && capture.expected_begin.view_id == self.identity.view_id
                && capture.expected_begin.owner == self.identity.owner
                && capture.expected_begin.generation <= i64::MAX as u64
                && self.stop.execution_id == self.identity.execution_id
                && self.stop.operation_id == self.identity.operation_id
                && self.stop.view_id == self.identity.view_id
                && self.stop.owner == self.identity.owner
                && self.stop.run_id == self.identity.run_id
                && self.stop.run_epoch == self.identity.run_epoch
                && self.evidence_pin
                    == format!(
                        "execution:evidence:{}:{}",
                        self.identity.execution_id, self.freeze.request.intent_id
                    ),
            "remote evidence reservation differs from its exact execution/stop/freeze"
        );
        Ok(())
    }
    pub(super) fn references(&self) -> Vec<ObjectRef> {
        let mut refs = vec![self.freeze_reference.clone(), self.drain_record.clone()];
        refs.sort();
        refs
    }
    pub(super) fn digest(&self) -> Result<String> {
        self.validate_data()?;
        Ok(hex::encode(Sha256::digest(
            crate::protocol::canonical_json(self)?,
        )))
    }
}
/// Persisted before receiving either Blob or performing the one fresh capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteEvidenceLease {
    pub format_version: u32,
    pub reservation_digest: String,
    pub staging: StagingLease,
}
/// Historical snapshots only. A saved receipt may have since been released;
/// current retention always requires the store's exact generation comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFinalizationRecovery {
    pub reservation: RemoteFinalizationReservation,
    pub lease: Option<RemoteEvidenceLease>,
    pub receipt: Option<ObjectSetReceipt>,
}
/// Bounded off-actor read. Missing records return None; malformed or unsafe
/// evidence remains an error. It never creates/chmods/repairs execution state.
pub fn read_execution_recovery(
    root: &Path,
    execution_id: &str,
) -> Result<Option<HistoricalExecution>> {
    #[cfg(target_os = "linux")]
    {
        let Some(record) = super::state::read_recovery_record(root, execution_id)? else {
            return Ok(None);
        };
        let remote_evidence = super::state::read_evidence_recovery(root, &record)?;
        Ok(Some(HistoricalExecution {
            record,
            remote_evidence,
        }))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, execution_id);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "execution recovery reader requires the Linux execution profile",
        )
        .into())
    }
}

enum Kind {
    Local,
    Remote(RemoteLaunchRequest),
    RemoteFinalizable(RemoteLaunchRequest),
}
pub struct OwnedHelperWorkspace {
    // Drop view/manager before the resource owner. Neither accessors nor a
    // checkpoint return value transfer the supervisor's resource obligation.
    view: VfsView,
    manager: VfsManager,
    kind: Kind,
    _resources: Box<dyn HelperWorkspaceResources>,
}
impl OwnedHelperWorkspace {
    /// Existing local implementation, with no fork, inferred checkout or eager
    /// preparation. The selected view must already exist under this owner.
    pub fn local(request: &NewExecution<'_>) -> Result<Self> {
        ensure!(
            !request
                .spec
                .profile
                .as_ref()
                .is_some_and(|profile| matches!(
                    &profile.workspace,
                    super::ExecutionWorkspaceSelectionV4::RemoteSourceOnly { .. }
                )),
            "remote selection cannot fall back to the local workspace factory"
        );
        let manager = VfsManager::open(
            &request.spec.manager_root,
            LocalStore::open(&request.spec.store_root)?,
        )?;
        let view = manager.open_view(&request.spec.view_id, &request.spec.owner)?;
        Self::from_local(request, manager, view, ())
    }
    pub fn from_local<R: HelperWorkspaceResources + 'static>(
        request: &NewExecution<'_>,
        manager: VfsManager,
        view: VfsView,
        resources: R,
    ) -> Result<Self> {
        ensure!(
            !request
                .spec
                .profile
                .as_ref()
                .is_some_and(|profile| matches!(
                    &profile.workspace,
                    super::ExecutionWorkspaceSelectionV4::RemoteSourceOnly { .. }
                )),
            "remote selection cannot acquire complete local checkpoint capability"
        );
        ensure!(
            manager.remote_root_binding().is_none()
                && view.manager().remote_root_binding().is_none(),
            "remote workspace cannot acquire complete local checkpoint capability"
        );
        validate_workspace(request, &manager, &view)?;
        Ok(Self {
            view,
            manager,
            kind: Kind::Local,
            _resources: Box::new(resources),
        })
    }
    /// Consume the pager's opaque validated selection. Only an exact spec4
    /// selection and namespace5 can acquire partial-finalization capability.
    pub fn from_remote<R: HelperWorkspaceResources + 'static>(
        request: &NewExecution<'_>,
        workspace: RemoteLaunchWorkspace,
        resources: R,
    ) -> Result<Self> {
        let (manager, view, remote) = workspace.into_parts();
        validate_workspace(request, &manager, &view)?;
        let spec = request.spec();
        if let Some(profile) = &spec.profile {
            ensure!(
                matches!(&profile.workspace,
                super::ExecutionWorkspaceSelectionV4::RemoteSourceOnly { selection, .. } if selection == &remote),
                "remote factory result differs from the sealed v4 workspace selection"
            );
            ensure!(
                view.descriptor()?.remote_finalization_format == Some(1),
                "profile4 remote workspace requires finalizable namespace5"
            );
        }
        let observation = spec
            .workspace_observation
            .as_ref()
            .context("remote workspace requires an exact sealed observation")?;
        let broker = spec
            .broker_binding
            .as_ref()
            .context("remote workspace requires an exact sealed run binding")?;
        ensure!(
            remote.observation == observation.binding
                && remote.attempt.project_id == broker.project_id
                && remote.attempt.run_id == broker.run_id
                && remote.attempt.run_epoch == broker.run_epoch
                && remote.attempt.owner == spec.owner
                && remote.attempt.execution_id == spec.execution_id
                && remote.attempt.operation_id == spec.operation_id
                && remote.attempt.view_id == spec.view_id
                && manager.remote_root_binding() == Some(&remote.root)
                && view.manager().remote_root_binding() == Some(&remote.root),
            "remote factory result differs from the locked execution selection"
        );
        Ok(Self {
            view,
            manager,
            kind: if spec.format_version == 4 && spec.profile.is_some() {
                Kind::RemoteFinalizable(remote)
            } else {
                Kind::Remote(remote)
            },
            _resources: Box::new(resources),
        })
    }
    pub fn manager(&self) -> &VfsManager {
        &self.manager
    }
    pub fn view(&self) -> &VfsView {
        &self.view
    }
    pub fn remote_request(&self) -> Option<&RemoteLaunchRequest> {
        match &self.kind {
            Kind::Local => None,
            Kind::Remote(request) | Kind::RemoteFinalizable(request) => Some(request),
        }
    }
    pub fn checkpoint_capability(&self) -> CheckpointCapability {
        match self.kind {
            Kind::Local => CheckpointCapability::CompleteLocal,
            Kind::Remote(_) => CheckpointCapability::PartialRemoteUnavailable,
            Kind::RemoteFinalizable(_) => CheckpointCapability::PartialRemoteFinalizable,
        }
    }
    /// Legacy remote factory results remain preparation-only. Spec4's actual
    /// physical wiring must separately validate its stronger capability.
    pub(crate) fn require_supported_execution(&self) -> Result<()> {
        if self.checkpoint_capability() == CheckpointCapability::PartialRemoteUnavailable {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "remote workspace execution requires quiescent partial finalization",
            )
            .into());
        }
        Ok(())
    }
}
fn validate_workspace(
    request: &NewExecution<'_>,
    manager: &VfsManager,
    view: &VfsView,
) -> Result<()> {
    let spec = request.spec();
    let descriptor = view.descriptor()?;
    ensure!(
        manager.root().canonicalize()? == request.manager_root
            && manager.store().root().canonicalize()? == request.store_root
            && view.manager().root() == manager.root()
            && view.manager().store().root() == manager.store().root()
            && descriptor.view_id == spec.view_id
            && descriptor.owner == spec.owner,
        "factory workspace differs from reserved manager/store/view/owner"
    );
    if let Some(observation) = &spec.workspace_observation {
        ensure!(
            view.dirty_cursor()?.namespace == observation.binding.namespace,
            "factory workspace differs from reserved observation namespace"
        );
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::execution::{
        record::{encode_envelope, ExecutionRecord, ExecutionRecordV1},
        state::{self, Preflight, State},
        BrokerBinding, ExecutionPhase, HelperServices, HelperServicesFactory,
        WorkspaceObservationProfile,
    };
    use crate::{
        protocol::{canonical_json, ObjectKind, ObjectRef, OwnerEpoch},
        store::ImportPolicy,
        vfs::{
            rpc::{AttemptAuthority, AttemptContext, ObservationBinding},
            VfsCheckpoint,
        },
    };
    use fs2::FileExt;
    use std::{
        collections::BTreeMap,
        fs::{self, OpenOptions},
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
    };
    use uuid::Uuid;

    struct Fixture {
        root: tempfile::TempDir,
        spec: ExecutionSpec,
    }
    impl Fixture {
        fn new(observed: bool) -> Self {
            let root = tempfile::Builder::new()
                .prefix("ew-")
                .tempdir_in("/tmp")
                .unwrap();
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let base = root.path().canonicalize().unwrap();
            let execution_id = Uuid::new_v4().to_string();
            let operation_id = Uuid::new_v4().to_string();
            let view_id = Uuid::new_v4().to_string();
            let owner = OwnerEpoch {
                owner: "workspace-fixture".into(),
                epoch: 3,
            };
            let mut spec = ExecutionSpec {
                format_version: 1,
                execution_id: execution_id.clone(),
                operation_id: operation_id.clone(),
                view_id: view_id.clone(),
                owner: owner.clone(),
                execution_root: base.join("executions"),
                manager_root: base.join("manager"),
                store_root: base.join("cas"),
                authority_roots: vec![base.join("authority")],
                command: vec!["/bin/true".into()],
                environment: BTreeMap::new(),
                runtime: vec![],
                home_files: vec![],
                broker_directory: None,
                broker_binding: None,
                workspace_observation: None,
                profile: None,
                timeout_seconds: 30,
            };
            if observed {
                let broker = base.join("broker");
                fs::create_dir(&broker).unwrap();
                fs::set_permissions(&broker, fs::Permissions::from_mode(0o700)).unwrap();
                fs::write(broker.join("identity.json"),br#"{"format_version":1,"project_id":"project","run_id":"run","run_epoch":1,"owner_epoch":3,"token":"fixture-token-not-a-live-grant"}"#).unwrap();
                fs::set_permissions(
                    broker.join("identity.json"),
                    fs::Permissions::from_mode(0o600),
                )
                .unwrap();
                spec.format_version = 2;
                spec.broker_directory = Some(broker);
                spec.broker_binding = Some(BrokerBinding {
                    project_id: "project".into(),
                    run_id: "run".into(),
                    run_epoch: 1,
                    owner_epoch: 3,
                });
                spec.workspace_observation = Some(WorkspaceObservationProfile {
                    binding: ObservationBinding {
                        format_version: 1,
                        attempt: AttemptContext {
                            execution_id,
                            operation_id,
                        },
                        view_id,
                        owner,
                        namespace: Uuid::new_v4().to_string(),
                        profile: ObjectRef::from_bytes(ObjectKind::Blob, b"fixture profile"),
                        exclusion_policy_digest: "a".repeat(64),
                    },
                    endpoint_directory: base.join("workspace"),
                    application: None,
                });
            }
            spec.validate().unwrap();
            Self { root, spec }
        }
        fn prepare_local(&self) -> VfsCheckpoint {
            let source = self.root.path().join("source");
            fs::create_dir(&source).unwrap();
            fs::write(source.join("file"), b"unchanged local fixture").unwrap();
            let manager = VfsManager::open(
                &self.spec.manager_root,
                LocalStore::open(&self.spec.store_root).unwrap(),
            )
            .unwrap();
            let checkpoint = manager
                .prepare_from_quiesced_path(&source, &ImportPolicy::default())
                .unwrap();
            drop(
                manager
                    .fork_snapshot(&self.spec.view_id, &checkpoint, self.spec.owner.clone())
                    .unwrap(),
            );
            checkpoint
        }
    }
    struct Allow;
    impl AttemptAuthority for Allow {
        fn validate(&self, _: &ObservationBinding) -> Result<()> {
            Ok(())
        }
    }
    fn services(counter: Arc<AtomicUsize>, workspace: Arc<AtomicUsize>) -> HelperServicesFactory {
        Box::new(move |_| {
            counter.fetch_add(1, Ordering::AcqRel);
            Ok(HelperServices {
                workspace: super::super::HelperWorkspaceServices::Complete {
                    authority: Arc::new(Allow),
                    adoption_witness: None,
                },
                application: None,
                workspace_factory: Some(Box::new(move |_| {
                    workspace.fetch_add(1, Ordering::AcqRel);
                    anyhow::bail!("fixture workspace setup failure")
                })),
            })
        })
    }
    #[test]
    fn duplicate_and_retried_execution_cannot_repeat_service_or_workspace_effects() {
        let fixture = Fixture::new(true);
        let service_calls = Arc::new(AtomicUsize::new(0));
        let workspace_calls = Arc::new(AtomicUsize::new(0));
        let Preflight::New {
            mut state,
            mut services,
        } = state::preflight(
            &fixture.spec,
            Some(services(service_calls.clone(), workspace_calls.clone())),
        )
        .unwrap()
        else {
            panic!("new fixture");
        };
        assert_eq!(service_calls.load(Ordering::Acquire), 1);
        assert!(state::preflight(
            &fixture.spec,
            Some(self::services(
                service_calls.clone(),
                workspace_calls.clone()
            ))
        )
        .is_err());
        assert_eq!(service_calls.load(Ordering::Acquire), 1);
        assert!(state
            .open_workspace(
                &fixture.spec,
                services.as_mut().unwrap().workspace_factory.take()
            )
            .is_err());
        let late = workspace_calls.clone();
        assert!(state
            .open_workspace(
                &fixture.spec,
                Some(Box::new(move |_| {
                    late.fetch_add(1, Ordering::AcqRel);
                    anyhow::bail!("repeated");
                }))
            )
            .is_err());
        assert_eq!(workspace_calls.load(Ordering::Acquire), 1);
        assert!(state.mark_running().is_err());
        drop(state);
        drop(services);
        assert!(state::preflight(
            &fixture.spec,
            Some(self::services(
                service_calls.clone(),
                workspace_calls.clone()
            ))
        )
        .is_err());
        assert_eq!(service_calls.load(Ordering::Acquire), 1);
        assert_eq!(workspace_calls.load(Ordering::Acquire), 1);
        assert!(!fixture.spec.manager_root.exists());
        assert!(!fixture.spec.store_root.exists());
    }
    #[test]
    fn every_recovered_phase_is_reconciled_before_services_and_keeps_the_lock() {
        for phase in [
            ExecutionPhase::Preparing,
            ExecutionPhase::Running,
            ExecutionPhase::Orphaned,
            ExecutionPhase::Checkpointed,
        ] {
            let fixture = Fixture::new(true);
            let state = State::open(&fixture.spec).unwrap();
            modify_record_fixture(&state, |record| {
                record.phase = phase;
                if phase == ExecutionPhase::Checkpointed {
                    record.agent_exit = Some(0);
                    record.checkpoint = Some(VfsCheckpoint {
                        format_version: 1,
                        directory: ObjectRef::from_bytes(
                            ObjectKind::Directory,
                            b"historical directory identity",
                        ),
                        hardlinks: ObjectRef::from_bytes(
                            ObjectKind::Blob,
                            b"historical topology identity",
                        ),
                    });
                }
            });
            let record = state.directory.join("record.json");
            let original = fs::read(&record).unwrap();
            drop(state);
            let calls = Arc::new(AtomicUsize::new(0));
            let result = state::preflight(
                &fixture.spec,
                Some(services(calls.clone(), Arc::new(AtomicUsize::new(0)))),
            );
            if phase == ExecutionPhase::Checkpointed {
                let Preflight::Recovered(recovered) = result.unwrap() else {
                    panic!("historical checkpoint must not be new");
                };
                assert!(State::open(&fixture.spec).is_err());
                assert_eq!(recovered.record().phase(), ExecutionPhase::Checkpointed);
                drop(recovered);
            } else {
                assert!(result.is_err());
            }
            assert_eq!(calls.load(Ordering::Acquire), 0);
            assert_eq!(fs::read(record).unwrap(), original);
            assert!(!fixture.spec.manager_root.exists());
            assert!(!fixture.spec.store_root.exists());
        }
    }
    #[test]
    fn changed_or_corrupt_execution_records_never_run_a_factory() {
        let fixture = Fixture::new(true);
        let state = State::open(&fixture.spec).unwrap();
        let record = state.directory.join("record.json");
        drop(state);
        let original = fs::read(&record).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut changed = fixture.spec.clone();
        changed.command.push("changed-input".into());
        assert!(state::preflight(
            &changed,
            Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
        )
        .is_err());
        assert_eq!(fs::read(&record).unwrap(), original);
        fs::write(&record, b"partial record").unwrap();
        assert!(
            read_execution_recovery(&fixture.spec.execution_root, &fixture.spec.execution_id)
                .is_err()
        );
        assert!(state::preflight(
            &fixture.spec,
            Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
        )
        .is_err());
        fs::remove_file(&record).unwrap();
        let other = fixture.root.path().join("other-record");
        fs::write(&other, &original).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&other, &record).unwrap();
        assert!(
            read_execution_recovery(&fixture.spec.execution_root, &fixture.spec.execution_id)
                .is_err()
        );
        assert!(state::preflight(
            &fixture.spec,
            Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
        )
        .is_err());
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert_eq!(fs::read(other).unwrap(), original);
    }
    #[test]
    fn failed_services_leave_a_durable_orphan_and_cannot_retry_effects() {
        let fixture = Fixture::new(true);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        assert!(state::preflight(
            &fixture.spec,
            Some(Box::new(move |_| {
                count.fetch_add(1, Ordering::AcqRel);
                anyhow::bail!("fixture services failure");
            }))
        )
        .is_err());
        let record = fixture
            .spec
            .execution_root
            .join("runs")
            .join(&fixture.spec.execution_id)
            .join("record.json");
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
        assert_eq!(saved["record"]["phase"], "orphaned");
        assert!(state::preflight(
            &fixture.spec,
            Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
        )
        .is_err());
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }
    #[test]
    fn missing_record_after_reservation_never_restarts_effects() {
        let fixture = Fixture::new(true);
        let state = State::open(&fixture.spec).unwrap();
        let record = state.directory.join("record.json");
        drop(state);
        fs::remove_file(&record).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let result = state::preflight(
            &fixture.spec,
            Some(services(calls.clone(), Arc::new(AtomicUsize::new(0)))),
        );
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert!(!record.exists());
        assert!(!fixture.spec.manager_root.exists());
        assert!(!fixture.spec.store_root.exists());
    }
    // Deliberate on-disk crash/corruption fixture, not an active State setter.
    fn modify_record_fixture(state: &State, modify: impl FnOnce(&mut ExecutionRecordV1)) {
        let ExecutionRecord::V1(mut record) = state.record().clone() else {
            panic!("legacy fixture");
        };
        modify(&mut record);
        fs::write(
            state.directory.join("record.json"),
            encode_envelope(&ExecutionRecord::V1(record)).unwrap(),
        )
        .unwrap();
    }
    fn complete_record(state: &State) {
        modify_record_fixture(state, |record| {
            record.phase = ExecutionPhase::Checkpointed;
            record.agent_exit = Some(0);
            record.checkpoint = Some(VfsCheckpoint {
                format_version: 1,
                directory: ObjectRef::from_bytes(
                    ObjectKind::Directory,
                    b"historical directory identity",
                ),
                hardlinks: ObjectRef::from_bytes(ObjectKind::Blob, b"historical topology identity"),
            });
        });
    }
    #[test]
    fn missing_lock_beside_retained_records_is_never_recreated() {
        for complete in [false, true] {
            let fixture = Fixture::new(true);
            let mut state = State::open(&fixture.spec).unwrap();
            if complete {
                complete_record(&mut state);
            }
            let directory = state.directory.clone();
            let record = fs::read(directory.join("record.json")).unwrap();
            let sidecar = fs::read(directory.join("lock-identity.json")).unwrap();
            drop(state);
            fs::remove_file(directory.join("owner.lock")).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            assert!(state::preflight(
                &fixture.spec,
                Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
            )
            .is_err());
            assert_eq!(calls.load(Ordering::Acquire), 0);
            assert!(!directory.join("owner.lock").exists());
            assert_eq!(fs::read(directory.join("record.json")).unwrap(), record);
            assert_eq!(
                fs::read(directory.join("lock-identity.json")).unwrap(),
                sidecar
            );
            let historical =
                read_execution_recovery(&fixture.spec.execution_root, &fixture.spec.execution_id)
                    .unwrap()
                    .unwrap();
            assert_eq!(
                historical.record().phase(),
                if complete {
                    ExecutionPhase::Checkpointed
                } else {
                    ExecutionPhase::Preparing
                }
            );
            assert_eq!(historical.record().checkpoint().is_some(), complete);
            assert!(!directory.join("owner.lock").exists());
        }
    }
    #[test]
    fn replaced_lock_cannot_compete_with_the_actual_held_supervisor() {
        let fixture = Fixture::new(true);
        let mut state = State::open(&fixture.spec).unwrap();
        complete_record(&mut state);
        let directory = state.directory.clone();
        let record = fs::read(directory.join("record.json")).unwrap();
        fs::rename(
            directory.join("owner.lock"),
            directory.join("original-owned.lock"),
        )
        .unwrap();
        let mut replacement = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("owner.lock"))
            .unwrap();
        std::io::Write::write_all(&mut replacement, Uuid::new_v4().to_string().as_bytes()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(state::preflight(
            &fixture.spec,
            Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
        )
        .is_err());
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert!(state.mark_running().is_err());
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.join("original-owned.lock"))
            .unwrap();
        assert!(contender.try_lock_exclusive().is_err());
        assert_eq!(fs::read(directory.join("record.json")).unwrap(), record);
        assert_eq!(
            read_execution_recovery(&fixture.spec.execution_root, &fixture.spec.execution_id)
                .unwrap()
                .unwrap()
                .record()
                .phase(),
            ExecutionPhase::Checkpointed
        );
        drop(state);
        contender.try_lock_exclusive().unwrap();
        FileExt::unlock(&contender).unwrap();
    }
    #[test]
    fn pre_sidecar_crashes_and_malformed_sidecars_are_historical_only() {
        for damage in [
            "lock-only",
            "sidecar-without-record",
            "record-without-sidecar",
            "partial-sidecar",
            "wrong-format",
            "wrong-owner",
            "wrong-inode",
            "wrong-nonce",
        ] {
            let fixture = Fixture::new(true);
            let mut state = State::open(&fixture.spec).unwrap();
            complete_record(&mut state);
            let directory = state.directory.clone();
            let record = fs::read(directory.join("record.json")).unwrap();
            drop(state);
            let sidecar_path = directory.join("lock-identity.json");
            match damage {
                "lock-only" => {
                    fs::remove_file(&sidecar_path).unwrap();
                    fs::remove_file(directory.join("record.json")).unwrap();
                }
                "sidecar-without-record" => fs::remove_file(directory.join("record.json")).unwrap(),
                "record-without-sidecar" => fs::remove_file(&sidecar_path).unwrap(),
                "partial-sidecar" => {
                    fs::write(&sidecar_path, b"partial immutable identity").unwrap()
                }
                field => {
                    let mut value: serde_json::Value =
                        serde_json::from_slice(&fs::read(&sidecar_path).unwrap()).unwrap();
                    match field {
                        "wrong-format" => value["format_version"] = 2.into(),
                        "wrong-owner" => value["owner"]["owner"] = "different-owner".into(),
                        "wrong-inode" => value["inode"] = 0.into(),
                        "wrong-nonce" => value["nonce"] = Uuid::new_v4().to_string().into(),
                        _ => unreachable!(),
                    }
                    fs::write(&sidecar_path, canonical_json(&value).unwrap()).unwrap();
                }
            }
            let saved_sidecar = fs::read(&sidecar_path).ok();
            let calls = Arc::new(AtomicUsize::new(0));
            assert!(
                state::preflight(
                    &fixture.spec,
                    Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
                )
                .is_err(),
                "{damage}"
            );
            assert_eq!(calls.load(Ordering::Acquire), 0, "{damage}");
            assert_eq!(fs::read(&sidecar_path).ok(), saved_sidecar, "{damage}");
            let historical =
                read_execution_recovery(&fixture.spec.execution_root, &fixture.spec.execution_id)
                    .unwrap();
            if matches!(damage, "lock-only" | "sidecar-without-record") {
                assert!(historical.is_none());
            } else {
                assert_eq!(
                    historical.unwrap().record().phase(),
                    ExecutionPhase::Checkpointed
                );
                assert_eq!(fs::read(directory.join("record.json")).unwrap(), record);
            }
            assert!(!fixture.spec.manager_root.exists());
        }
    }
    #[test]
    fn restored_copies_retain_records_but_cannot_recreate_execution_authority() {
        let fixture = Fixture::new(true);
        let mut state = State::open(&fixture.spec).unwrap();
        complete_record(&mut state);
        let directory = state.directory.clone();
        let originals: Vec<_> = ["owner.lock", "lock-identity.json", "record.json"]
            .into_iter()
            .map(|name| (name, fs::read(directory.join(name)).unwrap()))
            .collect();
        drop(state);
        fs::rename(&directory, directory.with_extension("backup")).unwrap();
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        for (name, bytes) in &originals {
            fs::write(directory.join(name), bytes).unwrap();
            fs::set_permissions(directory.join(name), fs::Permissions::from_mode(0o600)).unwrap();
        }
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(state::preflight(
            &fixture.spec,
            Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
        )
        .is_err());
        assert_eq!(calls.load(Ordering::Acquire), 0);
        for (name, bytes) in originals {
            assert_eq!(fs::read(directory.join(name)).unwrap(), bytes);
        }
        assert_eq!(
            read_execution_recovery(&fixture.spec.execution_root, &fixture.spec.execution_id)
                .unwrap()
                .unwrap()
                .record()
                .phase(),
            ExecutionPhase::Checkpointed
        );
    }
    #[test]
    fn workspace_result_cannot_outlive_replaced_lock_authority() {
        let fixture = Fixture::new(false);
        fixture.prepare_local();
        let mut state = State::open(&fixture.spec).unwrap();
        let directory = state.directory.clone();
        let record = fs::read(directory.join("record.json")).unwrap();
        let result = state.open_workspace(
            &fixture.spec,
            Some(Box::new(move |request| {
                let workspace = OwnedHelperWorkspace::local(request)?;
                fs::rename(
                    request.directory().join("owner.lock"),
                    request.directory().join("retained-owned.lock"),
                )?;
                let mut replacement = OpenOptions::new()
                    .create_new(true)
                    .read(true)
                    .write(true)
                    .mode(0o600)
                    .open(request.directory().join("owner.lock"))?;
                std::io::Write::write_all(&mut replacement, Uuid::new_v4().to_string().as_bytes())?;
                Ok(workspace)
            })),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(directory.join("record.json")).unwrap(), record);
        assert!(!directory.join("root").exists());
    }
    #[test]
    fn late_lock_replacement_prevents_dirty_checkpoint_and_cas_retention_effects() {
        fn object_bytes(store: &LocalStore) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
            let mut objects = BTreeMap::new();
            for shard in fs::read_dir(store.root().join("objects")).unwrap() {
                for object in fs::read_dir(shard.unwrap().path()).unwrap() {
                    let path = object.unwrap().path();
                    objects.insert(
                        path.strip_prefix(store.root()).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    );
                }
            }
            objects
        }
        let fixture = Fixture::new(false);
        fixture.prepare_local();
        let mut state = State::open(&fixture.spec).unwrap();
        let workspace = state.open_workspace(&fixture.spec, None).unwrap();
        let file = workspace.view().lookup(1, b"file").unwrap();
        let dirty = b"dirty bytes that must not enter a checkpoint after custody loss";
        workspace.view().write(file.ino, 0, dirty, true).unwrap();
        let store = workspace.manager().store();
        let retention = store.retention().unwrap();
        let objects = object_bytes(store);
        let record = fs::read(state.directory.join("record.json")).unwrap();
        fs::rename(
            state.directory.join("owner.lock"),
            state.directory.join("retained-owned.lock"),
        )
        .unwrap();
        let mut replacement = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(state.directory.join("owner.lock"))
            .unwrap();
        std::io::Write::write_all(&mut replacement, Uuid::new_v4().to_string().as_bytes()).unwrap();
        assert!(state.checkpoint_workspace(&workspace).is_err());
        assert_eq!(store.retention().unwrap(), retention);
        assert_eq!(object_bytes(store), objects);
        assert_eq!(
            fs::read(state.directory.join("record.json")).unwrap(),
            record
        );
        assert_eq!(
            workspace.view().read(file.ino, 0, dirty.len()).unwrap(),
            dirty
        );
        assert!(state.record().checkpoint().is_none());
    }
    #[test]
    fn unsafe_existing_state_directories_are_refused_without_permission_repair() {
        use std::os::unix::fs::MetadataExt;
        let fixture = Fixture::new(true);
        let state = State::open(&fixture.spec).unwrap();
        let directory = state.directory.clone();
        drop(state);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(state::preflight(
            &fixture.spec,
            Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
        )
        .is_err());
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert_eq!(
            fs::symlink_metadata(&directory).unwrap().mode() & 0o777,
            0o755
        );
        assert!(
            read_execution_recovery(&fixture.spec.execution_root, &fixture.spec.execution_id)
                .is_err()
        );
    }
    #[test]
    fn incomplete_checkpoint_records_never_become_recovered_or_new() {
        for missing_exit in [false, true] {
            let fixture = Fixture::new(true);
            let state = State::open(&fixture.spec).unwrap();
            modify_record_fixture(&state, |record| {
                record.phase = ExecutionPhase::Checkpointed;
                if missing_exit {
                    record.checkpoint = Some(VfsCheckpoint {
                        format_version: 1,
                        directory: ObjectRef::from_bytes(
                            ObjectKind::Directory,
                            b"historical directory",
                        ),
                        hardlinks: ObjectRef::from_bytes(ObjectKind::Blob, b"historical topology"),
                    });
                } else {
                    record.agent_exit = Some(0);
                }
            });
            let record = state.directory.join("record.json");
            let original = fs::read(&record).unwrap();
            drop(state);
            let calls = Arc::new(AtomicUsize::new(0));
            assert!(state::preflight(
                &fixture.spec,
                Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
            )
            .is_err());
            assert_eq!(calls.load(Ordering::Acquire), 0);
            assert_eq!(fs::read(&record).unwrap(), original);
            assert!(!fixture.spec.manager_root.exists());
        }
    }
    #[test]
    fn local_fallback_preserves_spec_bytes_and_current_checkpoint_behavior() {
        let fixture = Fixture::new(false);
        let initial = fixture.prepare_local();
        let bytes = canonical_json(&fixture.spec).unwrap();
        let Preflight::New {
            mut state,
            services,
        } = state::preflight(&fixture.spec, None).unwrap()
        else {
            panic!("new fixture");
        };
        assert!(services.is_none());
        let workspace = state.open_workspace(&fixture.spec, None).unwrap();
        assert_eq!(
            workspace.checkpoint_capability(),
            CheckpointCapability::CompleteLocal
        );
        assert!(workspace.remote_request().is_none());
        assert_eq!(workspace.view().id(), fixture.spec.view_id);
        assert_eq!(workspace.manager().root(), fixture.spec.manager_root);
        let checkpoint = state.checkpoint_workspace(&workspace).unwrap();
        assert_eq!(checkpoint.format_version, 1);
        assert_eq!(checkpoint.directory.kind, ObjectKind::Directory);
        assert_eq!(checkpoint.hardlinks.kind, ObjectKind::Blob);
        assert_eq!(checkpoint.directory, initial.directory);
        assert_eq!(checkpoint.hardlinks, initial.hardlinks);
        assert_eq!(canonical_json(&fixture.spec).unwrap(), bytes);
    }
    struct Guard {
        lock: File,
        record: std::path::PathBuf,
        published: Arc<AtomicBool>,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let value = fs::read(&self.record)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            self.published.store(
                value.is_some_and(|value| {
                    value["record"]["phase"] == "checkpointed"
                        && !value["record"]["checkpoint"].is_null()
                }),
                Ordering::Release,
            );
            let _ = FileExt::unlock(&self.lock);
        }
    }
    #[test]
    fn owned_resources_keep_the_real_guard_through_checkpoint_and_record_fsync() {
        let fixture = Fixture::new(false);
        fixture.prepare_local();
        let mut state = State::open(&fixture.spec).unwrap();
        let path = fixture.root.path().join("resource.lock");
        let lock = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        lock.lock_exclusive().unwrap();
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let published = Arc::new(AtomicBool::new(false));
        let guard = Guard {
            lock,
            record: state.directory.join("record.json"),
            published: published.clone(),
        };
        let workspace = state
            .open_workspace(
                &fixture.spec,
                Some(Box::new(move |request| {
                    let manager = VfsManager::open(
                        request.manager_root(),
                        LocalStore::open(request.store_root())?,
                    )?;
                    let view = manager.open_view(&request.spec().view_id, &request.spec().owner)?;
                    OwnedHelperWorkspace::from_local(request, manager, view, guard)
                })),
            )
            .unwrap();
        assert!(contender.try_lock_exclusive().is_err());
        assert!(!published.load(Ordering::Acquire));
        state.mark_running().unwrap();
        let committed = state.finalize_local(&workspace, 0).unwrap();
        assert_eq!(committed.record().phase(), ExecutionPhase::Checkpointed);
        assert!(contender.try_lock_exclusive().is_err());
        drop(workspace);
        assert!(published.load(Ordering::Acquire));
        contender.try_lock_exclusive().unwrap();
        FileExt::unlock(&contender).unwrap();
    }

    #[test]
    fn state_transitions_commit_data_once_and_historical_state_cannot_restart() {
        let fixture = Fixture::new(false);
        fixture.prepare_local();
        let mut state = State::open(&fixture.spec).unwrap();
        let initial = fs::read(state.directory.join("record.json")).unwrap();
        assert!(state.mark_running().is_err());
        assert_eq!(
            fs::read(state.directory.join("record.json")).unwrap(),
            initial
        );
        let workspace = state.open_workspace(&fixture.spec, None).unwrap();
        state.mark_running().unwrap();
        assert_eq!(state.record().phase(), ExecutionPhase::Running);
        assert!(state.mark_running().is_err());
        let committed = state.finalize_local(&workspace, 143).unwrap();
        state.verify_commit_current(&committed).unwrap();
        assert_eq!(committed.record().agent_exit(), Some(143));
        assert!(matches!(
            committed.record().checkpointed_data().unwrap(),
            crate::execution::CheckpointRef::CompleteLocal(_)
        ));
        let retention = workspace.manager().store().retention().unwrap();
        assert!(state.finalize_local(&workspace, 0).is_err());
        assert!(state.mark_orphaned().is_err());
        assert_eq!(workspace.manager().store().retention().unwrap(), retention);
        drop(workspace);
        drop(state);
        let Preflight::Recovered(mut recovered) = state::preflight(&fixture.spec, None).unwrap()
        else {
            panic!("historical only");
        };
        assert!(!recovered.is_new());
        assert!(recovered.mark_running().is_err());
        assert!(recovered.verify_commit_current(&committed).is_err());
        let historical = recovered.into_historical();
        assert_eq!(historical.record().phase(), ExecutionPhase::Checkpointed);
    }
    #[test]
    fn failed_local_finalization_does_not_replay_capture_after_handles_settle() {
        let fixture = Fixture::new(false);
        fixture.prepare_local();
        let mut state = State::open(&fixture.spec).unwrap();
        let workspace = state.open_workspace(&fixture.spec, None).unwrap();
        let file = workspace.view().lookup(1, b"file").unwrap();
        let handle = workspace
            .view()
            .open_handle(file.ino, libc::O_RDONLY, false)
            .unwrap();
        state.mark_running().unwrap();
        let retention = workspace.manager().store().retention().unwrap();
        assert!(state.finalize_local(&workspace, 0).is_err());
        workspace.view().close_handle(file.ino, handle).unwrap();
        assert!(state.finalize_local(&workspace, 0).is_err());
        assert_eq!(workspace.manager().store().retention().unwrap(), retention);
        state.mark_orphaned().unwrap();
        assert_eq!(state.record().phase(), ExecutionPhase::Orphaned);
        assert!(state.record().checkpoint().is_none());
    }
    #[test]
    fn committed_data_is_rechecked_against_the_persisted_envelope_before_publication() {
        let fixture = Fixture::new(false);
        fixture.prepare_local();
        let mut state = State::open(&fixture.spec).unwrap();
        let workspace = state.open_workspace(&fixture.spec, None).unwrap();
        state.mark_running().unwrap();
        let committed = state.finalize_local(&workspace, 0).unwrap();
        state.verify_commit_current(&committed).unwrap();
        let original = fs::read(state.directory.join("record.json")).unwrap();
        modify_record_fixture(&state, |record| record.agent_exit = Some(143));
        assert!(state.verify_commit_current(&committed).is_err());
        fs::write(state.directory.join("record.json"), original).unwrap();
        state.verify_commit_current(&committed).unwrap();
        fs::remove_file(state.directory.join("record.json")).unwrap();
        assert!(state.verify_commit_current(&committed).is_err());
    }
    #[test]
    fn record2_history_is_visible_but_cannot_downgrade_into_a_legacy_factory() {
        use crate::execution::{
            CheckpointArtifactV2, ExecutionIdentityV2, ExecutionRecordV2, ExecutionStateV2,
        };
        let fixture = Fixture::new(true);
        let state = State::open(&fixture.spec).unwrap();
        let old = state.record();
        let record = ExecutionRecord::V2(ExecutionRecordV2 {
            format_version: 2,
            identity: ExecutionIdentityV2 {
                project_id: "project".into(),
                request_id: Uuid::new_v4().to_string(),
                run_id: "run".into(),
                run_epoch: 1,
                execution_id: old.execution_id().into(),
                operation_id: old.operation_id().into(),
                view_id: old.view_id().into(),
                owner: old.owner().clone(),
                request_digest: old.request_digest().into(),
                supervisor_pid: old.supervisor_pid(),
            },
            state: ExecutionStateV2::Checkpointed {
                agent_exit: 0,
                checkpoint: CheckpointArtifactV2::CompleteLocal(VfsCheckpoint {
                    format_version: 1,
                    directory: ObjectRef::from_bytes(
                        ObjectKind::Directory,
                        b"historical v2 directory",
                    ),
                    hardlinks: ObjectRef::from_bytes(ObjectKind::Blob, b"historical topology"),
                }),
            },
        });
        let path = state.directory.join("record.json");
        fs::write(&path, encode_envelope(&record).unwrap()).unwrap();
        drop(state);
        let original = fs::read(&path).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(state::preflight(
            &fixture.spec,
            Some(services(calls.clone(), Arc::new(AtomicUsize::new(0))))
        )
        .is_err());
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert_eq!(fs::read(&path).unwrap(), original);
        let historical =
            read_execution_recovery(&fixture.spec.execution_root, &fixture.spec.execution_id)
                .unwrap()
                .unwrap();
        assert!(matches!(historical.record(), ExecutionRecord::V2(_)));
        assert!(!fixture.spec.manager_root.exists());
        assert!(!fixture.spec.store_root.exists());
    }

    struct Source {
        scope: crate::remote_cache::CacheScope,
        objects: std::sync::Mutex<BTreeMap<ObjectRef, Vec<u8>>>,
        receipt: std::sync::Mutex<Option<crate::remote_source::ReplicatedRootReceipt>>,
    }
    impl Source {
        fn put(&self, kind: ObjectKind, bytes: &[u8]) -> ObjectRef {
            let reference = ObjectRef::from_bytes(kind, bytes);
            self.objects
                .lock()
                .unwrap()
                .insert(reference.clone(), bytes.to_vec());
            reference
        }
        fn metadata(&self, kind: ObjectKind, value: &impl serde::Serialize) -> ObjectRef {
            self.put(kind, &canonical_json(value).unwrap())
        }
    }
    impl crate::directory::DirectoryStore for Source {
        fn read(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
            self.objects
                .lock()
                .unwrap()
                .get(reference)
                .cloned()
                .context("fixture object missing")
        }
        fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
            Ok(Source::put(self, kind, bytes))
        }
    }
    impl crate::remote_cache::ImmutableObjectSource for Source {
        fn scope(&self) -> crate::remote_cache::CacheScope {
            self.scope.clone()
        }
        fn fetch(
            &self,
            reference: &ObjectRef,
            control: &crate::remote_cache::FetchControl,
            output: &mut dyn std::io::Write,
        ) -> Result<()> {
            control.check()?;
            output.write_all(&crate::directory::DirectoryStore::read(self, reference)?)?;
            control.check()?;
            Ok(())
        }
    }
    impl crate::remote_root::RootReceiptSource for Source {
        fn scope(&self) -> crate::remote_cache::CacheScope {
            self.scope.clone()
        }
        fn get_root(
            &self,
            _: &str,
            _: &crate::remote_source::SourceRequestControl,
        ) -> std::result::Result<
            crate::remote_source::ReplicatedRootReceipt,
            crate::remote_source::SourceError,
        > {
            self.receipt
                .lock()
                .unwrap()
                .clone()
                .ok_or(crate::remote_source::SourceError::RemoteFailed)
        }
    }
    struct ExactAuthority {
        attempt: crate::remote_root::AttemptBinding,
        root: crate::remote_root::RootBinding,
    }
    impl crate::remote_root::CurrentAttemptAuthority for ExactAuthority {
        fn validate(
            &self,
            attempt: &crate::remote_root::AttemptBinding,
            root: &crate::remote_root::RootBinding,
        ) -> Result<()> {
            ensure!(
                attempt == &self.attempt && root == &self.root,
                "fixture authority binding mismatch"
            );
            Ok(())
        }
    }
    #[test]
    fn actual_partial_remote_result_has_no_local_fallback_or_checkpoint() {
        use crate::{
            directory::PagedDirectory,
            protocol::CheckpointConsistency,
            remote_cache::{CacheLimits, CacheScope, RemoteCache},
            remote_root::{AttemptBinding, RemoteRootRegistry, RootBinding, RootLimits},
            remote_source::{ReplicatedDurability, ReplicatedRootReceipt, SourceRequestControl},
            snapshot::{RunnableRoot, SourceV2},
            vfs::{exclusion_policy_digest, remote_launch, RemoteOptions},
            workspace::WorkspaceProfile,
        };
        let mut fixture = Fixture::new(true);
        let source = Arc::new(Source {
            scope: CacheScope {
                provider_id: "execution-fixture".into(),
                tenant_id: Uuid::new_v4().to_string(),
            },
            objects: Default::default(),
            receipt: Default::default(),
        });
        let directory = PagedDirectory::new(source.as_ref())
            .build(0o755, [])
            .unwrap();
        let seed = PagedDirectory::new(source.as_ref())
            .build(0o700, [])
            .unwrap();
        let profile = source.metadata(ObjectKind::Blob, &WorkspaceProfile::source_only());
        let policy = source.put(ObjectKind::Blob, br#"{"format_version":1}"#);
        let accepted_source = source.metadata(
            ObjectKind::SourceRoot,
            &SourceV2 {
                format_version: 2,
                consistency: CheckpointConsistency::Quiesced,
                directory: directory.clone(),
                projection_policy: policy.clone(),
            },
        );
        let hardlinks = source.put(ObjectKind::Blob, br#"{"format_version":1,"groups":[]}"#);
        let runnable = source.metadata(
            ObjectKind::RunnableRoot,
            &RunnableRoot {
                format_version: 1,
                consistency: CheckpointConsistency::Quiesced,
                source: accepted_source.clone(),
                environment: None,
                view_root: directory,
                profile: profile.clone(),
                hardlinks,
                knowledge_cursor: 0,
            },
        );
        let root = RootBinding {
            scope: source.scope.clone(),
            root_id: "fixture-root".into(),
            root: runnable,
        };
        let (object_count, total_bytes) = {
            let objects = source.objects.lock().unwrap();
            (
                objects.len() as u64,
                objects.values().map(|bytes| bytes.len() as u64).sum(),
            )
        };
        *source.receipt.lock().unwrap() = Some(ReplicatedRootReceipt {
            format_version: 1,
            root_id: root.root_id.clone(),
            root: root.root.clone(),
            durability: ReplicatedDurability::Replicated,
            published_at_ms: 1,
            object_count,
            total_bytes,
        });
        let observation = &mut fixture.spec.workspace_observation.as_mut().unwrap().binding;
        observation.profile = profile;
        observation.exclusion_policy_digest =
            exclusion_policy_digest(&ImportPolicy::default()).unwrap();
        let attempt = AttemptBinding {
            project_id: "project".into(),
            run_id: "run".into(),
            run_epoch: 1,
            owner: fixture.spec.owner.clone(),
            execution_id: fixture.spec.execution_id.clone(),
            operation_id: fixture.spec.operation_id.clone(),
            view_id: fixture.spec.view_id.clone(),
        };
        let selection = RemoteLaunchRequest {
            format_version: 1,
            request_id: Uuid::new_v4().to_string(),
            root: root.clone(),
            attempt: attempt.clone(),
            observation: observation.clone(),
            projection_policy: policy,
            accepted_source,
            accepted_seed: seed,
        };
        let mut state = State::open(&fixture.spec).unwrap();
        let base = fixture.root.path().to_path_buf();
        let workspace = state
            .open_workspace(
                &fixture.spec,
                Some(Box::new(move |request| {
                    for name in ["cache", "registry"] {
                        fs::create_dir(base.join(name))?;
                        fs::set_permissions(base.join(name), fs::Permissions::from_mode(0o700))?;
                    }
                    let cache = RemoteCache::open(
                        base.join("cache"),
                        source.clone(),
                        CacheLimits::default(),
                    )?;
                    let registry = RemoteRootRegistry::open(
                        base.join("registry"),
                        cache,
                        source,
                        Arc::new(ExactAuthority {
                            attempt: attempt.clone(),
                            root: root.clone(),
                        }),
                        RootLimits::default(),
                    )?;
                    let session = registry.admit(
                        &attempt,
                        &root.root_id,
                        &root.root,
                        &SourceRequestControl::new(
                            std::time::Instant::now() + std::time::Duration::from_secs(3),
                        ),
                    )?;
                    let remote = remote_launch::validate_selection(
                        session,
                        &selection,
                        RemoteOptions::default(),
                    )?
                    .open(
                        request.manager_root(),
                        LocalStore::open(request.store_root())?,
                    )?;
                    assert!(OwnedHelperWorkspace::from_local(
                        request,
                        remote.manager().clone(),
                        remote.view().clone(),
                        ()
                    )
                    .is_err());
                    OwnedHelperWorkspace::from_remote(request, remote, ())
                })),
            )
            .unwrap();
        assert_eq!(
            workspace.checkpoint_capability(),
            CheckpointCapability::PartialRemoteUnavailable
        );
        assert_eq!(
            workspace.remote_request().unwrap().attempt.owner,
            fixture.spec.owner
        );
        let error = workspace.require_supported_execution().unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::Unsupported
        );
        assert!(state.checkpoint_workspace(&workspace).is_err());
        assert!(!state.directory.join("root").exists());
        assert!(!state.directory.join("home").exists());
        assert!(!fixture
            .spec
            .manager_root
            .join("mounts")
            .join(&fixture.spec.view_id)
            .exists());
        assert_eq!(state.record().phase(), ExecutionPhase::Preparing);
        assert!(state.record().checkpoint().is_none());
    }
}
