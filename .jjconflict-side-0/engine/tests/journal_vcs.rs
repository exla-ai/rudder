use rudder_engine::journal::*;
use rudder_engine::protocol::{
    canonical_json, CheckpointConsistency, Durability, ObjectKind, ObjectRef,
};
use rudder_engine::store::{
    CheckpointManifest, DirectoryManifest, EnvironmentManifest, LocalStore, SourceManifest,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

const NOW: i64 = 1000;
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEXT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Fixture {
    dir: tempfile::TempDir,
    db: PathBuf,
    journal: Journal,
    store: LocalStore,
    owner: OwnerFence,
    token: String,
    request: VcsIntentRequest,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("journal.sqlite");
        let mut journal = Journal::open(&db).unwrap();
        let owner = journal
            .create_project("project", "owner", BASE, NOW)
            .unwrap();
        journal
            .observe_run_attempt(&owner, "source", "source-attempt", NOW)
            .unwrap();
        let store = LocalStore::open(dir.path().join("cas")).unwrap();
        let directory = store
            .put_directory(DirectoryManifest {
                format_version: 1,
                mode: 0o755,
                entries: vec![],
            })
            .unwrap();
        let source = store
            .put_source(&SourceManifest {
                format_version: 1,
                directory: directory.clone(),
                parents: vec![],
                consistency: CheckpointConsistency::Quiesced,
            })
            .unwrap();
        let environment = store
            .put_environment(&EnvironmentManifest {
                format_version: 1,
                directory: directory.clone(),
                platform: [("abi".into(), "test-host".into())].into_iter().collect(),
                installer_inputs: vec![],
                provenance: BTreeMap::new(),
            })
            .unwrap();
        let checkpoint = store
            .put_checkpoint(&CheckpointManifest {
                format_version: 1,
                source: source.clone(),
                environment: Some(environment.clone()),
                parents: vec![],
                consistency: CheckpointConsistency::Quiesced,
                durability: Durability::Local,
                knowledge_cursor: 0,
            })
            .unwrap();
        let policy = store.put_blob(b"versioned-source-policy").unwrap();
        let request = VcsIntentRequest {
            idempotency_key: "land".into(),
            source_run: VcsRunFence {
                run_id: "source".into(),
                run_epoch: 1,
                attempt_id: "source-attempt".into(),
                view_id: "view".into(),
            },
            expected_head: BASE.into(),
            proposed_source_root: source.clone(),
            proposed_environment_root: environment.clone(),
            recovery_root: checkpoint,
            seed_root: directory,
            projection_policy: policy.clone(),
            profile: policy,
            expected_view_root: None,
            hardlinks: None,
            versions: [("api".into(), 0)].into_iter().collect(),
            evidence: IntegrationEvidence {
                tested_head: BASE.into(),
                tested_source_root: source,
                tested_environment_root: environment,
                action_results: vec![],
                checks: json!({"verified":"empty semantic fixture"}),
            },
        };
        let token = journal
            .issue_capability(
                &owner,
                &CapabilityGrant {
                    principal: "source-principal".into(),
                    run_id: "source".into(),
                    run_epoch: 1,
                    operations: ["decision:accept".into()].into_iter().collect(),
                    recipients: BTreeMap::new(),
                    scopes: ["project".into()].into_iter().collect(),
                    topics: ["api".into()].into_iter().collect(),
                    expires_at_ms: None,
                },
                NOW,
            )
            .unwrap();
        Self {
            dir,
            db,
            journal,
            store,
            owner,
            token,
            request,
        }
    }
    fn accepted(&mut self) -> VcsIntentReceipt {
        self.journal
            .begin_vcs_intent(&self.owner, &self.store, &self.request, NOW)
            .unwrap()
    }
    fn prepared(&mut self) -> VcsIntentReceipt {
        let accepted = self.accepted();
        let prepared = envelope(&self.store, &accepted);
        self.journal
            .attach_vcs_operation(
                &self.owner,
                &self.store,
                &accepted.intent_id,
                &prepared,
                NOW,
            )
            .unwrap()
    }
}

fn envelope(store: &LocalStore, receipt: &VcsIntentReceipt) -> PreparedVcsOperation {
    let request = &receipt.request;
    let tree = json!({"terms":["d".repeat(40)],"labels":[]});
    let baseline = json!({"commit":BASE,"change":"e".repeat(32),"parents":[],"tree":tree});
    let proof = json!({"format_version":1,"binding":{"format_version":1,"project_id":receipt.project_id,"owner":{"owner":receipt.original_owner.owner_id,"epoch":receipt.original_owner.epoch},"run_id":request.source_run.run_id,"run_epoch":request.source_run.run_epoch,"attempt_id":request.source_run.attempt_id,"view_id":request.source_run.view_id,"seed":request.seed_root,"baseline":baseline,"baseline_operation":"1".repeat(128),"projection_policy":request.projection_policy,"profile":request.profile},"recovery_root":request.recovery_root,"source":request.proposed_source_root,"environment":request.proposed_environment_root,"tree":tree});
    let proof_identity = store.put_blob(&canonical_json(&proof).unwrap()).unwrap();
    let record = json!({"format_version":1,"intent_id":receipt.intent_id,"expected_heads":["1".repeat(128)],"operation":"2".repeat(128),"integration_base":BASE,"commit":{"commit":NEXT,"change":"c".repeat(32),"parents":[BASE],"tree":tree},"proof":proof});
    PreparedVcsOperation {
        format_version: 1,
        record: store.put_blob(&canonical_json(&record).unwrap()).unwrap(),
        operation_id: "2".repeat(128),
        expected_operation_heads: vec!["1".repeat(128)],
        commit_id: NEXT.into(),
        proof_identity,
    }
}

struct Publisher {
    marker: PathBuf,
    db: PathBuf,
    intent_id: String,
    calls: AtomicUsize,
    crash_after_effect: bool,
    ambiguous: bool,
    changed: bool,
}
impl Publisher {
    fn new(f: &Fixture, receipt: &VcsIntentReceipt) -> Self {
        Self {
            marker: f.dir.path().join("published"),
            db: f.db.clone(),
            intent_id: receipt.intent_id.clone(),
            calls: AtomicUsize::new(0),
            crash_after_effect: false,
            ambiguous: false,
            changed: false,
        }
    }
}
impl VcsPublisher for Publisher {
    fn inspect(&mut self, _: &PreparedVcsOperation) -> anyhow::Result<VcsPublicationObservation> {
        if self.changed {
            return Ok(VcsPublicationObservation::HeadChanged {
                actual_heads: vec!["3".repeat(128)],
            });
        }
        Ok(if self.marker.exists() {
            VcsPublicationObservation::Published
        } else {
            VcsPublicationObservation::NotPublished
        })
    }
    fn publish(&mut self, _: &PreparedVcsOperation) -> anyhow::Result<VcsPublicationObservation> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Another connection sees the COMMITTED start even while the final
        // IMMEDIATE transaction owns the write gate.
        let db = rusqlite::Connection::open(&self.db)?;
        let state: String = db.query_row(
            "SELECT state FROM vcs_intents WHERE intent_id=?1",
            [&self.intent_id],
            |r| r.get(0),
        )?;
        assert_eq!(state, "started");
        std::fs::write(&self.marker, b"exact prepared operation published")?;
        std::fs::File::open(&self.marker)?.sync_all()?;
        std::fs::File::open(self.marker.parent().unwrap())?.sync_all()?;
        if self.crash_after_effect {
            std::process::exit(73);
        }
        if self.ambiguous {
            anyhow::bail!("lost callback receipt after effect");
        }
        Ok(VcsPublicationObservation::Published)
    }
}

#[test]
fn durable_intent_precedes_preparation_and_retains_exact_inputs_and_prepared_records() {
    let mut f = Fixture::new();
    let accepted = f.accepted();
    assert_eq!(accepted.state, VcsIntentState::Accepted);
    assert!(accepted.prepared.is_none());
    let retained = f.store.retention().unwrap();
    for (pin, root) in &accepted.retention_pins {
        assert_eq!(retained.pins.get(pin), Some(root));
    }
    let prepared = envelope(&f.store, &accepted);
    let result = f
        .journal
        .attach_vcs_operation(&f.owner, &f.store, &accepted.intent_id, &prepared, NOW)
        .unwrap();
    assert_eq!(result.state, VcsIntentState::Prepared);
    assert!(result
        .retention_pins
        .values()
        .any(|r| r == &prepared.record));
    assert!(result
        .retention_pins
        .values()
        .any(|r| r == &prepared.proof_identity));
    assert_eq!(
        f.journal
            .attach_vcs_operation(&f.owner, &f.store, &accepted.intent_id, &prepared, NOW)
            .unwrap(),
        result
    );
    let mut other = prepared;
    other.commit_id = "f".repeat(40);
    assert!(f
        .journal
        .attach_vcs_operation(&f.owner, &f.store, &accepted.intent_id, &other, NOW)
        .is_err());
}

#[test]
fn legacy_and_vcs_integrations_cannot_steal_each_others_idempotency_keys() {
    fn legacy(f: &Fixture) -> IntegrationRequest {
        IntegrationRequest {
            idempotency_key: f.request.idempotency_key.clone(),
            expected_head: BASE.into(),
            new_head: BASE.into(),
            proposed_source_root: f.request.proposed_source_root.clone(),
            proposed_environment_root: f.request.proposed_environment_root.clone(),
            versions: f.request.versions.clone(),
            evidence: f.request.evidence.clone(),
        }
    }
    let mut f = Fixture::new();
    let request = legacy(&f);
    f.accepted();
    assert!(f.journal.integrate(&f.owner, &request, NOW).is_err());
    let mut g = Fixture::new();
    let request = legacy(&g);
    g.journal.integrate(&g.owner, &request, NOW).unwrap();
    assert!(g
        .journal
        .begin_vcs_intent(&g.owner, &g.store, &g.request, NOW)
        .is_err());
}

#[test]
fn successful_publication_and_receipts_survive_owner_and_source_restart_without_replay() {
    let mut f = Fixture::new();
    let prepared = f.prepared();
    let mut publisher = Publisher::new(&f, &prepared);
    let completed = f
        .journal
        .publish_vcs_intent(&f.owner, &prepared.intent_id, &mut publisher, NOW)
        .unwrap();
    assert_eq!(completed.state, VcsIntentState::Published);
    assert_eq!(
        f.journal.project_state("project").unwrap().integration_head,
        NEXT
    );
    let owner = f
        .journal
        .transfer_owner(&f.owner, "new-owner", NOW)
        .unwrap();
    assert_eq!(
        f.journal
            .begin_vcs_intent(&owner, &f.store, &f.request, NOW)
            .unwrap(),
        completed
    );
    assert_eq!(
        f.journal
            .publish_vcs_intent(&owner, &prepared.intent_id, &mut publisher, NOW)
            .unwrap(),
        completed
    );
    assert_eq!(publisher.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn stale_attempt_or_relevant_decision_rejects_before_external_start() {
    for source_changed in [true, false] {
        let mut f = Fixture::new();
        let prepared = f.prepared();
        if source_changed {
            f.journal
                .observe_run_attempt(&f.owner, "source", "replacement-attempt", NOW)
                .unwrap();
        } else {
            f.journal
                .accept_decision(
                    &f.token,
                    &DecisionRequest {
                        idempotency_key: "changed".into(),
                        topic: "api".into(),
                        expected_version: 0,
                        scope: "project".into(),
                        statement: json!({"changed":true}),
                    },
                    NOW,
                )
                .unwrap();
        }
        let mut publisher = Publisher::new(&f, &prepared);
        assert!(f
            .journal
            .publish_vcs_intent(&f.owner, &prepared.intent_id, &mut publisher, NOW)
            .is_err());
        assert_eq!(publisher.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            f.journal
                .vcs_intent(&f.owner, &prepared.intent_id)
                .unwrap()
                .state,
            VcsIntentState::Prepared
        );
    }
}

#[test]
fn ambiguous_outcome_blocks_legacy_integration_and_reconciles_without_replay() {
    let mut f = Fixture::new();
    let prepared = f.prepared();
    let mut publisher = Publisher::new(&f, &prepared);
    publisher.ambiguous = true;
    let uncertain = f
        .journal
        .publish_vcs_intent(&f.owner, &prepared.intent_id, &mut publisher, NOW)
        .unwrap();
    assert_eq!(uncertain.state, VcsIntentState::Uncertain);
    assert_eq!(
        f.journal.project_state("project").unwrap().integration_head,
        BASE
    );
    let legacy = IntegrationRequest {
        idempotency_key: "bypass".into(),
        expected_head: BASE.into(),
        new_head: "legacy-other".into(),
        proposed_source_root: f.request.proposed_source_root.clone(),
        proposed_environment_root: f.request.proposed_environment_root.clone(),
        versions: f.request.versions.clone(),
        evidence: f.request.evidence.clone(),
    };
    assert!(f.journal.integrate(&f.owner, &legacy, NOW).is_err());
    assert!(f
        .journal
        .publish_vcs_intent(&f.owner, &prepared.intent_id, &mut publisher, NOW)
        .is_err());
    let reconciled = f
        .journal
        .reconcile_vcs_intent(&f.owner, &prepared.intent_id, &mut publisher, NOW)
        .unwrap();
    assert_eq!(reconciled.state, VcsIntentState::Published);
    assert_eq!(
        reconciled.result.as_ref().unwrap()["historical_reconciliation"],
        true
    );
    assert_eq!(publisher.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn published_with_new_decisions_stays_uncertain_and_changed_heads_are_not_no_effect() {
    let mut f = Fixture::new();
    let prepared = f.prepared();
    let mut publisher = Publisher::new(&f, &prepared);
    publisher.ambiguous = true;
    f.journal
        .publish_vcs_intent(&f.owner, &prepared.intent_id, &mut publisher, NOW)
        .unwrap();
    f.journal
        .accept_decision(
            &f.token,
            &DecisionRequest {
                idempotency_key: "later".into(),
                topic: "api".into(),
                expected_version: 0,
                scope: "project".into(),
                statement: json!({"new":true}),
            },
            NOW,
        )
        .unwrap();
    let result = f
        .journal
        .reconcile_vcs_intent(&f.owner, &prepared.intent_id, &mut publisher, NOW)
        .unwrap();
    assert_eq!(result.state, VcsIntentState::Uncertain);
    assert_eq!(result.result.unwrap()["observed"], "published");
    assert_eq!(
        f.journal.project_state("project").unwrap().integration_head,
        BASE
    );
    publisher.changed = true;
    let changed = f
        .journal
        .reconcile_vcs_intent(&f.owner, &prepared.intent_id, &mut publisher, NOW)
        .unwrap();
    assert_eq!(changed.state, VcsIntentState::Uncertain);
    assert_eq!(publisher.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn malformed_prepared_binding_and_missing_retention_never_attach() {
    let mut f = Fixture::new();
    let accepted = f.accepted();
    let prepared = envelope(&f.store, &accepted);
    let mut altered = prepared.clone();
    altered.operation_id = "4".repeat(128);
    assert!(f
        .journal
        .attach_vcs_operation(&f.owner, &f.store, &accepted.intent_id, &altered, NOW)
        .is_err());
    altered = prepared;
    altered.proof_identity = ObjectRef::from_bytes(ObjectKind::Blob, b"missing");
    assert!(f
        .journal
        .attach_vcs_operation(&f.owner, &f.store, &accepted.intent_id, &altered, NOW)
        .is_err());
    assert_eq!(
        f.journal
            .vcs_intent(&f.owner, &accepted.intent_id)
            .unwrap()
            .state,
        VcsIntentState::Accepted
    );
    let mut bad_request = f.request.clone();
    bad_request.idempotency_key = "missing-root".into();
    bad_request.recovery_root = ObjectRef::from_bytes(ObjectKind::Checkpoint, b"not in CAS");
    assert!(f
        .journal
        .begin_vcs_intent(&f.owner, &f.store, &bad_request, NOW)
        .is_err());
}

#[test]
fn projection_record_v2_requires_exact_preaccepted_typed_evidence_and_keeps_legacy_v1() {
    let mut f = Fixture::new();
    // This journal fixture checks retention/identity admission only. The real
    // bridge fixture independently checks the actual mapping receipt payload.
    let projection = f.store.put_metadata(ObjectKind::ActionResult, &rudder_engine::store::ReferencedMetadata {
        format_version: 1,
        references: vec![f.request.proposed_source_root.clone(), f.request.projection_policy.clone()],
        payload: json!({"kind":"source-jj-projection-v1","fixture":"journal-admission-only"}),
    }).unwrap();
    f.request.evidence.action_results.push(projection.clone());
    let accepted = f.accepted();
    let mut prepared = envelope(&f.store, &accepted);
    let mut record: serde_json::Value =
        serde_json::from_slice(&f.store.read_object(&prepared.record).unwrap()).unwrap();
    assert_eq!(record["format_version"], 1);
    assert!(record.get("projection_receipt").is_none());
    record["format_version"] = json!(2);
    prepared.record = f.store.put_blob(&canonical_json(&record).unwrap()).unwrap();
    assert!(f
        .journal
        .attach_vcs_operation(&f.owner, &f.store, &accepted.intent_id, &prepared, NOW)
        .is_err());
    record["projection_receipt"] = serde_json::to_value(&f.request.projection_policy).unwrap();
    prepared.record = f.store.put_blob(&canonical_json(&record).unwrap()).unwrap();
    assert!(f
        .journal
        .attach_vcs_operation(&f.owner, &f.store, &accepted.intent_id, &prepared, NOW)
        .is_err());
    let unaccepted = f
        .store
        .put_metadata(
            ObjectKind::ActionResult,
            &rudder_engine::store::ReferencedMetadata {
                format_version: 1,
                references: vec![],
                payload: json!({"kind":"unaccepted-projection"}),
            },
        )
        .unwrap();
    record["projection_receipt"] = serde_json::to_value(unaccepted).unwrap();
    prepared.record = f.store.put_blob(&canonical_json(&record).unwrap()).unwrap();
    assert!(f
        .journal
        .attach_vcs_operation(&f.owner, &f.store, &accepted.intent_id, &prepared, NOW)
        .is_err());
    record["projection_receipt"] = serde_json::to_value(&projection).unwrap();
    prepared.record = f.store.put_blob(&canonical_json(&record).unwrap()).unwrap();
    let receipt = f
        .journal
        .attach_vcs_operation(&f.owner, &f.store, &accepted.intent_id, &prepared, NOW)
        .unwrap();
    assert_eq!(receipt.state, VcsIntentState::Prepared);
    assert!(f
        .store
        .retention()
        .unwrap()
        .pins
        .values()
        .any(|root| root == &projection));
    let mut publisher = Publisher::new(&f, &receipt);
    let published = f
        .journal
        .publish_vcs_intent(&f.owner, &receipt.intent_id, &mut publisher, NOW)
        .unwrap();
    assert_eq!(published.state, VcsIntentState::Published);
}

#[test]
fn stored_runnable_source_environment_policy_view_profile_and_topology_must_match() {
    use rudder_engine::snapshot::{EnvironmentV2, RunnableRoot, SourceV2};
    let mut f = Fixture::new();
    let source = f
        .store
        .put_metadata(
            ObjectKind::SourceRoot,
            &SourceV2 {
                format_version: 2,
                consistency: CheckpointConsistency::Quiesced,
                directory: f.request.seed_root.clone(),
                projection_policy: f.request.projection_policy.clone(),
            },
        )
        .unwrap();
    let environment = f
        .store
        .put_metadata(
            ObjectKind::EnvironmentRoot,
            &EnvironmentV2 {
                format_version: 2,
                directory: f.request.seed_root.clone(),
                platform: [("abi".into(), "test".into())].into_iter().collect(),
                installer_inputs: vec![],
                provenance: BTreeMap::new(),
            },
        )
        .unwrap();
    let hardlinks = f
        .store
        .put_blob(br#"{"format_version":1,"groups":[]}"#)
        .unwrap();
    let root = f
        .store
        .put_metadata(
            ObjectKind::RunnableRoot,
            &RunnableRoot {
                format_version: 1,
                consistency: CheckpointConsistency::Quiesced,
                source: source.clone(),
                environment: Some(environment.clone()),
                view_root: f.request.seed_root.clone(),
                profile: f.request.profile.clone(),
                hardlinks: hardlinks.clone(),
                knowledge_cursor: 0,
            },
        )
        .unwrap();
    f.request.proposed_source_root = source.clone();
    f.request.evidence.tested_source_root = source;
    f.request.proposed_environment_root = environment.clone();
    f.request.evidence.tested_environment_root = environment;
    f.request.recovery_root = root;
    f.request.expected_view_root = Some(f.request.seed_root.clone());
    f.request.hardlinks = Some(hardlinks);
    let other_blob = f.store.put_blob(b"different declaration").unwrap();
    let other_dir = f
        .store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o700,
            entries: vec![],
        })
        .unwrap();
    for field in [
        "policy",
        "profile",
        "view",
        "topology",
        "source",
        "environment",
    ] {
        let mut changed = f.request.clone();
        changed.idempotency_key = field.into();
        match field {
            "policy" => changed.projection_policy = other_blob.clone(),
            "profile" => changed.profile = other_blob.clone(),
            "view" => changed.expected_view_root = Some(other_dir.clone()),
            "topology" => changed.hardlinks = Some(other_blob.clone()),
            "source" => {
                changed.proposed_source_root = f
                    .store
                    .put_source(&SourceManifest {
                        format_version: 1,
                        directory: other_dir.clone(),
                        parents: vec![],
                        consistency: CheckpointConsistency::Quiesced,
                    })
                    .unwrap();
                changed.evidence.tested_source_root = changed.proposed_source_root.clone();
            }
            _ => {
                changed.proposed_environment_root = f
                    .store
                    .put_metadata(
                        ObjectKind::EnvironmentRoot,
                        &EnvironmentV2 {
                            format_version: 2,
                            directory: other_dir.clone(),
                            platform: [("abi".into(), "other".into())].into_iter().collect(),
                            installer_inputs: vec![],
                            provenance: BTreeMap::new(),
                        },
                    )
                    .unwrap();
                changed.evidence.tested_environment_root =
                    changed.proposed_environment_root.clone();
            }
        }
        assert!(
            f.journal
                .begin_vcs_intent(&f.owner, &f.store, &changed, NOW)
                .is_err(),
            "stored {field} mismatch must fail"
        );
    }
    assert_eq!(f.accepted().state, VcsIntentState::Accepted);
}

#[test]
fn legacy_checkpoint_view_profile_and_available_topology_cannot_be_substituted() {
    use rudder_engine::store::{CheckpointV2Manifest, CheckpointV3Manifest};
    for version in [2, 3] {
        let mut f = Fixture::new();
        let topology = f
            .store
            .put_blob(br#"{"format_version":1,"groups":[]}"#)
            .unwrap();
        let root = if version == 2 {
            f.store
                .put_checkpoint_v2(&CheckpointV2Manifest {
                    format_version: 2,
                    source: f.request.proposed_source_root.clone(),
                    environment: Some(f.request.proposed_environment_root.clone()),
                    parents: vec![],
                    consistency: CheckpointConsistency::Quiesced,
                    durability: Durability::Local,
                    knowledge_cursor: 0,
                    view_root: f.request.seed_root.clone(),
                    profile: f.request.profile.clone(),
                })
                .unwrap()
        } else {
            f.store
                .put_checkpoint_v3(&CheckpointV3Manifest {
                    format_version: 3,
                    source: f.request.proposed_source_root.clone(),
                    environment: Some(f.request.proposed_environment_root.clone()),
                    parents: vec![],
                    consistency: CheckpointConsistency::Quiesced,
                    durability: Durability::Local,
                    knowledge_cursor: 0,
                    view_root: f.request.seed_root.clone(),
                    profile: f.request.profile.clone(),
                    hardlinks: topology.clone(),
                })
                .unwrap()
        };
        f.request.recovery_root = root;
        f.request.expected_view_root = Some(f.request.seed_root.clone());
        f.request.hardlinks = (version == 3).then_some(topology);
        let mut changed = f.request.clone();
        changed.profile = f.store.put_blob(b"substituted profile").unwrap();
        assert!(f
            .journal
            .begin_vcs_intent(&f.owner, &f.store, &changed, NOW)
            .is_err());
        assert_eq!(f.accepted().state, VcsIntentState::Accepted);
    }
}

#[test]
fn decision_writer_is_serialized_until_external_effect_and_integration_receipt_commit() {
    use std::sync::mpsc;
    use std::time::Duration;
    struct GatedPublisher {
        inner: Publisher,
        go: mpsc::Sender<()>,
        started: mpsc::Receiver<()>,
        done: mpsc::Receiver<()>,
    }
    impl VcsPublisher for GatedPublisher {
        fn inspect(
            &mut self,
            p: &PreparedVcsOperation,
        ) -> anyhow::Result<VcsPublicationObservation> {
            self.inner.inspect(p)
        }
        fn publish(
            &mut self,
            p: &PreparedVcsOperation,
        ) -> anyhow::Result<VcsPublicationObservation> {
            self.go.send(()).unwrap();
            self.started.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(
                matches!(
                    self.done.recv_timeout(Duration::from_millis(50)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ),
                "decision write escaped the final transaction"
            );
            self.inner.publish(p)
        }
    }
    let mut f = Fixture::new();
    let prepared = f.prepared();
    let mut writer = Journal::open(&f.db).unwrap();
    let token = f.token.clone();
    let (go, begin) = mpsc::channel();
    let (started_tx, started) = mpsc::channel();
    let (done_tx, done) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        begin.recv().unwrap();
        started_tx.send(()).unwrap();
        writer
            .accept_decision(
                &token,
                &DecisionRequest {
                    idempotency_key: "race".into(),
                    topic: "api".into(),
                    expected_version: 0,
                    scope: "project".into(),
                    statement: json!({"after":"integration"}),
                },
                NOW,
            )
            .unwrap();
        done_tx.send(()).unwrap();
    });
    let mut publisher = GatedPublisher {
        inner: Publisher::new(&f, &prepared),
        go,
        started,
        done,
    };
    let receipt = f
        .journal
        .publish_vcs_intent(&f.owner, &prepared.intent_id, &mut publisher, NOW)
        .unwrap();
    assert_eq!(receipt.state, VcsIntentState::Published);
    assert_eq!(receipt.integration.unwrap().versions.get("api"), Some(&0));
    publisher.done.recv_timeout(Duration::from_secs(2)).unwrap();
    thread.join().unwrap();
    assert_eq!(
        f.journal
            .current_decision("project", "api")
            .unwrap()
            .unwrap()
            .version,
        1
    );
}

#[test]
fn publication_crash_child() {
    let Some(root) = std::env::var_os("RUDDER_VCS_CRASH_CHILD") else {
        return;
    };
    let root = PathBuf::from(root);
    let input: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("child.json")).unwrap()).unwrap();
    let owner: OwnerFence = serde_json::from_value(input["owner"].clone()).unwrap();
    let mut journal = Journal::open(root.join("journal.sqlite")).unwrap();
    let id = input["intent_id"].as_str().unwrap();
    let mut publisher = Publisher {
        marker: root.join("published"),
        db: root.join("journal.sqlite"),
        intent_id: id.into(),
        calls: AtomicUsize::new(0),
        crash_after_effect: true,
        ambiguous: false,
        changed: false,
    };
    journal
        .publish_vcs_intent(&owner, id, &mut publisher, NOW)
        .unwrap();
    panic!("child must exit in the external effect before transaction commit");
}

#[test]
fn hard_process_crash_after_effect_preserves_started_and_current_owner_adopts_historical_fact() {
    let mut f = Fixture::new();
    let prepared = f.prepared();
    std::fs::write(
        f.dir.path().join("child.json"),
        serde_json::to_vec(&json!({"owner":f.owner,"intent_id":prepared.intent_id})).unwrap(),
    )
    .unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "publication_crash_child", "--nocapture"])
        .env_clear()
        .env("RUDDER_VCS_CRASH_CHILD", f.dir.path())
        .current_dir(f.dir.path())
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(f.dir.path().join("published").exists());
    assert_eq!(
        f.journal
            .vcs_intent(&f.owner, &prepared.intent_id)
            .unwrap()
            .state,
        VcsIntentState::Started
    );
    assert_eq!(
        f.journal.project_state("project").unwrap().integration_head,
        BASE
    );
    let next = f
        .journal
        .transfer_owner(&f.owner, "replacement-owner", NOW)
        .unwrap();
    assert_eq!(f.journal.recover_vcs_intents(&next, NOW).unwrap().len(), 1);
    let mut publisher = Publisher::new(&f, &prepared);
    let result = f
        .journal
        .reconcile_vcs_intent(&next, &prepared.intent_id, &mut publisher, NOW)
        .unwrap();
    assert_eq!(result.state, VcsIntentState::Published);
    assert_eq!(result.integration.unwrap().owner_epoch, f.owner.epoch);
    assert_eq!(
        publisher.calls.load(Ordering::SeqCst),
        0,
        "recovery inspects rather than replays"
    );
}
