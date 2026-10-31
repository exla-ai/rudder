//! Exercise DATA/evidence ordering without manufacturing an actual Linux drain.
//! The full remote transition remains covered only by the physical qualifier.
use super::*;
use crate::{
    execution::workspace::read_execution_recovery,
    protocol::canonical_json,
    vfs::{DirtyCursor, PartialCaptureRequest, RemoteFreezeIntent, RemoteFreezeRequest},
};
use std::cell::Cell;

struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    state: State,
    store: LocalStore,
    reservation: RemoteFinalizationReservation,
    freeze: Vec<u8>,
    audit: Vec<u8>,
}
impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::Builder::new()
            .prefix("es-evidence-")
            .tempdir()
            .unwrap();
        let base = temporary.path().canonicalize().unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let spec = ExecutionSpec {
            format_version: 1,
            execution_id: Uuid::new_v4().to_string(),
            operation_id: Uuid::new_v4().to_string(),
            view_id: Uuid::new_v4().to_string(),
            owner: OwnerEpoch {
                owner: "evidence-fixture".into(),
                epoch: 9_007_199_254_740_993,
            },
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
        let mut state = State::open(&spec).unwrap();
        let identity = ExecutionIdentityV2 {
            project_id: "project".into(),
            request_id: Uuid::new_v4().to_string(),
            run_id: "run".into(),
            run_epoch: 9_007_199_254_740_995,
            execution_id: spec.execution_id.clone(),
            operation_id: spec.operation_id.clone(),
            view_id: spec.view_id.clone(),
            owner: spec.owner.clone(),
            request_digest: state.record.request_digest().into(),
            supervisor_pid: std::process::id(),
        };
        // Deliberate DATA fixture only, not a Spec4 admission or a drain token.
        state.record = ExecutionRecord::V2(ExecutionRecordV2 {
            format_version: 2,
            identity: identity.clone(),
            state: ExecutionStateV2::Running,
        });
        state.expects_remote = true;
        state.persist_record(&state.record).unwrap();
        let stop = StopAndCheckpoint {
            request_id: Uuid::new_v4().to_string(),
            execution_id: identity.execution_id.clone(),
            operation_id: identity.operation_id.clone(),
            view_id: identity.view_id.clone(),
            owner: identity.owner.clone(),
            run_id: identity.run_id.clone(),
            run_epoch: identity.run_epoch,
        };
        let intent = RemoteFreezeIntent {
            format_version: 1,
            launch_digest: "a".repeat(64),
            request: RemoteFreezeRequest {
                format_version: 1,
                intent_id: Uuid::new_v4().to_string(),
                capture: PartialCaptureRequest {
                    nonce: stop.request_id.clone(),
                    expected_head: None,
                    expected_begin: DirtyCursor {
                        view_id: identity.view_id.clone(),
                        owner: identity.owner.clone(),
                        namespace: Uuid::new_v4().to_string(),
                        generation: 9_007_199_254_740_997,
                    },
                },
            },
        };
        let freeze = canonical_json(&intent).unwrap();
        let audit =
            canonical_json(&serde_json::json!({"fixture_data_only":"not a physical drain"}))
                .unwrap();
        let reservation = RemoteFinalizationReservation {
            format_version: 1,
            identity: identity.clone(),
            evidence_pin: format!(
                "execution:evidence:{}:{}",
                identity.execution_id, intent.request.intent_id
            ),
            freeze: intent,
            freeze_reference: ObjectRef::from_bytes(ObjectKind::Blob, &freeze),
            drain_record: ObjectRef::from_bytes(ObjectKind::Blob, &audit),
            stop,
            agent_exit: 0,
        };
        let store = LocalStore::open(&spec.store_root).unwrap();
        Self {
            _root: temporary,
            root: spec.execution_root,
            state,
            store,
            reservation,
            freeze,
            audit,
        }
    }
    fn reserve_and_lease(&self) -> RemoteEvidenceLease {
        self.state
            .reserve_remote_evidence(&self.reservation)
            .unwrap();
        self.state
            .begin_remote_evidence_lease(&self.store, &self.reservation)
            .unwrap()
    }
    fn retain(
        &self,
        lease: &RemoteEvidenceLease,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<ObjectSetReceipt> {
        self.state.retain_remote_evidence(
            &self.store,
            &self.reservation,
            lease,
            &self.freeze,
            &self.audit,
            check,
        )
    }
    fn history(&self) -> super::HistoricalExecution {
        read_execution_recovery(&self.root, self.state.record.execution_id())
            .unwrap()
            .unwrap()
    }
}

#[test]
fn remote_evidence_reservation_and_saved_lease_precede_any_blob_effects() {
    let fixture = Fixture::new();
    let before = fixture.store.retention().unwrap();
    assert!(fixture
        .state
        .begin_remote_evidence_lease(&fixture.store, &fixture.reservation)
        .is_err());
    assert_eq!(fixture.store.retention().unwrap(), before);
    fixture
        .state
        .reserve_remote_evidence(&fixture.reservation)
        .unwrap();
    let reserved = fixture.history();
    let evidence = reserved.remote_evidence().unwrap();
    assert_eq!(evidence.reservation, fixture.reservation);
    assert!(evidence.lease.is_none() && evidence.receipt.is_none());
    assert!(fixture
        .state
        .reserve_remote_evidence(&fixture.reservation)
        .is_err());
    let lease = fixture
        .state
        .begin_remote_evidence_lease(&fixture.store, &fixture.reservation)
        .unwrap();
    assert_eq!(
        fixture.history().remote_evidence().unwrap().lease.as_ref(),
        Some(&lease)
    );
    assert!(fixture
        .store
        .read_object(&fixture.reservation.freeze_reference)
        .is_err());
    assert!(fixture
        .store
        .read_object(&fixture.reservation.drain_record)
        .is_err());
    assert!(fixture
        .store
        .object_set_receipt(&fixture.reservation.evidence_pin)
        .unwrap()
        .is_none());
    let mut changed = lease.clone();
    changed.reservation_digest = "b".repeat(64);
    assert!(fixture.retain(&changed, &|| Ok(())).is_err());
    assert!(fixture
        .store
        .read_object(&fixture.reservation.freeze_reference)
        .is_err());
}

#[test]
fn remote_evidence_failures_preserve_discoverable_staging_or_ack_without_terminal_claim() {
    for failed_check in 1..=4 {
        let fixture = Fixture::new();
        let lease = fixture.reserve_and_lease();
        let checks = Cell::new(0);
        assert!(
            fixture
                .retain(&lease, &|| {
                    checks.set(checks.get() + 1);
                    ensure!(checks.get() != failed_check, "fixture finalization fence");
                    Ok(())
                })
                .is_err(),
            "check {failed_check}"
        );
        assert_eq!(checks.get(), failed_check);
        let history = fixture.history();
        assert_eq!(history.record().phase(), ExecutionPhase::Running);
        assert!(history.record().checkpoint().is_none());
        let data = history.remote_evidence().unwrap();
        assert_eq!(data.lease.as_ref(), Some(&lease));
        let active = fixture
            .store
            .object_set_receipt(&fixture.reservation.evidence_pin)
            .unwrap();
        if failed_check == 4 {
            let receipt = active.unwrap();
            assert_eq!(data.receipt.as_ref(), Some(&receipt));
            assert_eq!(
                fixture.store.object_set_refs(&receipt).unwrap(),
                fixture.reservation.references()
            );
            assert!(!fixture
                .store
                .retention()
                .unwrap()
                .staging
                .contains_key(&lease.staging.id));
            fixture.store.gc(Duration::ZERO).unwrap();
            assert_eq!(
                fixture
                    .store
                    .read_object(&fixture.reservation.freeze_reference)
                    .unwrap(),
                fixture.freeze
            );
            assert_eq!(
                fixture
                    .store
                    .read_object(&fixture.reservation.drain_record)
                    .unwrap(),
                fixture.audit
            );
        } else {
            assert!(active.is_none() && data.receipt.is_none());
            assert!(fixture
                .store
                .retention()
                .unwrap()
                .staging
                .contains_key(&lease.staging.id));
            fixture.store.gc(Duration::ZERO).unwrap();
            if failed_check > 1 {
                assert!(fixture
                    .store
                    .read_object(&fixture.reservation.freeze_reference)
                    .is_ok());
            }
            if failed_check > 2 {
                assert!(fixture
                    .store
                    .read_object(&fixture.reservation.drain_record)
                    .is_ok());
            }
        }
    }
}

#[test]
fn remote_evidence_receipt_is_exact_and_aba_safe_and_historical_read_is_not_active_proof() {
    let fixture = Fixture::new();
    let lease = fixture.reserve_and_lease();
    let receipt = fixture.retain(&lease, &|| Ok(())).unwrap();
    fixture
        .state
        .check_remote_evidence(&fixture.store, &fixture.reservation, &lease, &receipt)
        .unwrap();
    let directory_bytes: Vec<_> = [
        REMOTE_RESERVATION_FILE,
        REMOTE_LEASE_FILE,
        REMOTE_RECEIPT_FILE,
    ]
    .into_iter()
    .map(|name| (name, fs::read(fixture.state.directory.join(name)).unwrap()))
    .collect();
    fixture.store.release_object_set(&receipt).unwrap();
    assert!(fixture
        .state
        .check_remote_evidence(&fixture.store, &fixture.reservation, &lease, &receipt)
        .is_err());
    let next_lease = fixture
        .store
        .begin_staging(Duration::from_secs(30))
        .unwrap();
    let next = fixture
        .store
        .retain_object_set(
            &next_lease.id,
            &fixture.reservation.evidence_pin,
            &fixture.reservation.references(),
        )
        .unwrap();
    assert!(next.generation > receipt.generation);
    assert!(fixture.store.object_set_refs(&receipt).is_err());
    assert!(fixture.store.release_object_set(&receipt).is_err());
    assert!(fixture
        .state
        .check_remote_evidence(&fixture.store, &fixture.reservation, &lease, &receipt)
        .is_err());
    assert_eq!(
        fixture
            .history()
            .remote_evidence()
            .unwrap()
            .receipt
            .as_ref(),
        Some(&receipt)
    );
    for (name, bytes) in directory_bytes {
        assert_eq!(fs::read(fixture.state.directory.join(name)).unwrap(), bytes);
    }
    assert!(fixture.state.record.checkpoint().is_none());
}

#[test]
fn remote_evidence_late_named_lock_replacement_prevents_further_blob_and_ack_effects() {
    let fixture = Fixture::new();
    let lease = fixture.reserve_and_lease();
    let checks = Cell::new(0);
    assert!(fixture
        .retain(&lease, &|| {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                fs::rename(
                    fixture.state.directory.join("owner.lock"),
                    fixture.state.directory.join("retired-owner.lock"),
                )?;
                fs::write(fixture.state.directory.join("owner.lock"), b"foreign-lock")?;
            }
            Ok(())
        })
        .is_err());
    assert_eq!(checks.get(), 2);
    assert!(fixture
        .store
        .read_object(&fixture.reservation.freeze_reference)
        .is_ok());
    assert!(fixture
        .store
        .read_object(&fixture.reservation.drain_record)
        .is_err());
    assert!(fixture
        .store
        .object_set_receipt(&fixture.reservation.evidence_pin)
        .unwrap()
        .is_none());
    assert!(fixture
        .store
        .retention()
        .unwrap()
        .staging
        .contains_key(&lease.staging.id));
    assert!(fixture.history().remote_evidence().unwrap().lease.is_some());
}

#[test]
fn remote_recovery_missing_malformed_foreign_and_noncanonical_evidence_remain_visible_errors() {
    let fixture = Fixture::new();
    let lease = fixture.reserve_and_lease();
    let reservation_path = fixture.state.directory.join(REMOTE_RESERVATION_FILE);
    let original = fs::read(&reservation_path).unwrap();
    for changed in [b"{".to_vec(), [original.clone(), b" ".to_vec()].concat()] {
        fs::write(&reservation_path, changed).unwrap();
        assert!(
            read_execution_recovery(&fixture.root, fixture.state.record.execution_id()).is_err()
        );
    }
    let mut foreign = fixture.reservation.clone();
    foreign.identity.request_id = Uuid::new_v4().to_string();
    fs::write(&reservation_path, canonical_json(&foreign).unwrap()).unwrap();
    assert!(read_execution_recovery(&fixture.root, fixture.state.record.execution_id()).is_err());
    fs::write(&reservation_path, &original).unwrap();
    fs::remove_file(&reservation_path).unwrap();
    assert!(read_execution_recovery(&fixture.root, fixture.state.record.execution_id()).is_err());
    fs::write(&reservation_path, &original).unwrap();
    fs::set_permissions(&reservation_path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fixture.history().remote_evidence().unwrap().lease.as_ref(),
        Some(&lease)
    );
    fs::remove_file(fixture.state.directory.join("record.json")).unwrap();
    assert!(read_execution_recovery(&fixture.root, fixture.state.record.execution_id()).is_err());
}

#[test]
fn historical_partial_data_cannot_disagree_with_reserved_freeze_cursor_or_previous_head() {
    use crate::{
        delta::PartialTreeReceipt,
        remote_cache::CacheScope,
        remote_root::{AttemptBinding, RootBinding},
        vfs::{rpc::AttemptContext, ObservationConsistency},
    };
    let mut fixture = Fixture::new();
    fixture.reservation.freeze.request.capture.expected_head = Some(Uuid::new_v4().to_string());
    fixture.freeze = canonical_json(&fixture.reservation.freeze).unwrap();
    fixture.reservation.freeze_reference = ObjectRef::from_bytes(ObjectKind::Blob, &fixture.freeze);
    let lease = fixture.reserve_and_lease();
    let evidence_retention = fixture.retain(&lease, &|| Ok(())).unwrap();
    let reservation = &fixture.reservation;
    let identity = &reservation.identity;
    let cursor = &reservation.freeze.request.capture.expected_begin;
    // Deliberately persisted audit DATA fixture. No actual drain/frozen owner
    // is constructed and no State finalizer or live outcome is called.
    let artifact = QuiescedPartialCheckpointV1 {
        format_version: 1,
        capture: PartialCaptureReceipt {
            format_version: 1,
            view_id: identity.view_id.clone(),
            owner: identity.owner.clone(),
            attempt: AttemptBinding {
                project_id: identity.project_id.clone(),
                run_id: identity.run_id.clone(),
                run_epoch: identity.run_epoch,
                owner: identity.owner.clone(),
                execution_id: identity.execution_id.clone(),
                operation_id: identity.operation_id.clone(),
                view_id: identity.view_id.clone(),
            },
            base: RootBinding {
                scope: CacheScope {
                    provider_id: "fixture".into(),
                    tenant_id: "fixture".into(),
                },
                root_id: "fixture".into(),
                root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"audit-only fixture root"),
            },
            begin: cursor.clone(),
            end: cursor.clone(),
            exclusion_policy_digest: "c".repeat(64),
            request_nonce: reservation.stop.request_id.clone(),
            previous_head: reservation.freeze.request.capture.expected_head.clone(),
            head_sequence: 2,
            retention: PartialTreeReceipt {
                format_version: 1,
                plan: ObjectRef::from_bytes(ObjectKind::Blob, b"audit-only fixture plan"),
                local: ObjectSetReceipt {
                    format_version: 1,
                    pin_id: "fixture-capture".into(),
                    staging_id: Uuid::new_v4().to_string(),
                    set_digest: "d".repeat(64),
                    generation: 1,
                    object_count: 1,
                    total_bytes: 32,
                },
            },
            consistency: ObservationConsistency::LiveBestEffort,
        },
        stop: reservation.stop.clone(),
        observation: ObservationBinding {
            format_version: 1,
            attempt: AttemptContext {
                execution_id: identity.execution_id.clone(),
                operation_id: identity.operation_id.clone(),
            },
            view_id: identity.view_id.clone(),
            owner: identity.owner.clone(),
            namespace: cursor.namespace.clone(),
            profile: ObjectRef::from_bytes(ObjectKind::Blob, b"audit-only fixture profile"),
            exclusion_policy_digest: "c".repeat(64),
        },
        execution_request_digest: identity.request_digest.clone(),
        freeze_intent: reservation.freeze_reference.clone(),
        drain_record: reservation.drain_record.clone(),
        evidence_retention,
    };
    let original = ExecutionRecord::V2(ExecutionRecordV2 {
        format_version: 2,
        identity: identity.clone(),
        state: ExecutionStateV2::Checkpointed {
            agent_exit: reservation.agent_exit,
            checkpoint: CheckpointArtifactV2::PartialRemote(artifact),
        },
    });
    let path = fixture.state.directory.join("record.json");
    fs::write(&path, encode_envelope(&original).unwrap()).unwrap();
    assert!(fixture
        .history()
        .remote_evidence()
        .unwrap()
        .receipt
        .is_some());
    for change_cursor in [true, false] {
        let mut changed = original.clone();
        let ExecutionRecord::V2(record) = &mut changed else {
            unreachable!()
        };
        let ExecutionStateV2::Checkpointed {
            checkpoint: CheckpointArtifactV2::PartialRemote(checkpoint),
            ..
        } = &mut record.state
        else {
            unreachable!()
        };
        if change_cursor {
            checkpoint.capture.begin.generation += 1;
            checkpoint.capture.end.generation += 1;
        } else {
            checkpoint.capture.previous_head = Some(Uuid::new_v4().to_string());
        }
        // The changed record remains valid standalone DATA with a fresh valid
        // checksum, so the recovery reservation comparison must catch it.
        changed.validate_metadata().unwrap();
        fs::write(&path, encode_envelope(&changed).unwrap()).unwrap();
        assert!(
            read_execution_recovery(&fixture.root, fixture.state.record.execution_id()).is_err()
        );
    }
    fs::write(&path, encode_envelope(&original).unwrap()).unwrap();
    assert!(fixture.history().remote_evidence().is_some());
}
