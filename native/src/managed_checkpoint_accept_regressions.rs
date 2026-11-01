//! Private DATA/real-jj regressions. These do not manufacture the production
//! SettledManagedCheckpoint: its actual FD receipt and original pane are mandatory.
use super::*;
use jj_lib::{op_store::RefTarget, ref_name::{RefName, WorkspaceName}, workspace::Workspace};
use rudder_engine::{journal::{PreparedVcsOperation, VcsPublicationObservation, VcsPublisher},
    snapshot::EnvironmentV2, store::{DirectoryEntry, DirectoryManifest, EntryNode}};
use rudder_vcs_bridge::{JjTreeIdentity, publication::INTEGRATION_BOOKMARK};
use std::{collections::BTreeMap, os::unix::fs::DirBuilderExt};

struct Fixture {
    directory: PathBuf, store: LocalStore, base: RunnableRoot, checkpoint: VfsCheckpoint,
    policy: ObjectRef, stage: String,
}
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("rudder-managed-accept-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&directory).unwrap();
        let store = LocalStore::open(directory.join("cas")).unwrap();
        let empty = store.put_directory(DirectoryManifest { format_version: 1, mode: 0o755, entries: vec![] }).unwrap();
        let policy = store.put_blob(b"controlled source-only projection policy").unwrap();
        let profile = store.put_blob(&canonical_json(&WorkspaceProfile::source_only()).unwrap()).unwrap();
        let source = store.put_metadata(ObjectKind::SourceRoot, &SourceV2 { format_version: 2,
            consistency: CheckpointConsistency::Quiesced, directory: empty.clone(), projection_policy: policy.clone() }).unwrap();
        let environment = store.put_metadata(ObjectKind::EnvironmentRoot, &EnvironmentV2 { format_version: 2,
            directory: empty.clone(), platform: [("abi".into(), "controlled-test".into())].into(),
            installer_inputs: vec![], provenance: BTreeMap::new() }).unwrap();
        let topology = store.put_blob(br#"{"format_version":1,"groups":[]}"#).unwrap();
        let base = RunnableRoot { format_version: 1, consistency: CheckpointConsistency::Quiesced,
            source, environment: Some(environment), view_root: empty, profile, hardlinks: topology.clone(), knowledge_cursor: 7 };
        let payload = store.put_blob(b"actual captured candidate bytes\n").unwrap();
        let changed = store.put_directory(DirectoryManifest { format_version: 1, mode: 0o755, entries: vec![
            DirectoryEntry { name_hex: "726573756c742e747874".into(), node: EntryNode::File { object: payload, mode: 0o644 } },
        ] }).unwrap();
        let checkpoint = VfsCheckpoint { format_version: 1, directory: changed, hardlinks: topology };
        let stage = store.begin_staging(Duration::from_secs(120)).unwrap().id;
        Self { directory, store, base, checkpoint, policy, stage }
    }
    fn candidate(&self) -> ManagedCheckpointCandidate {
        versioned_candidate(&self.store, &self.stage, &self.base, &self.checkpoint, &self.policy).unwrap()
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.directory); } }

#[test]
fn managed_acceptance_completion_metadata_refuses_wrong_epoch_attempt_digest_pid_and_missing_checkpoint() {
    let f = Fixture::new();
    let uuid = || uuid::Uuid::new_v4().to_string();
    let fence = crate::managed_launch::ManagedLaunchFence { owner: OwnerFence { project_id: "project".into(),
        owner_id: "owner".into(), epoch: 1 }, run_id: "run".into(), run_epoch: 1, attempt_id: uuid(), request_id: uuid() };
    let operation = uuid(); let view = uuid(); let digest = "a".repeat(64);
    let record = rudder_engine::execution::ExecutionRecordV1 { format_version: 1, execution_id: fence.attempt_id.clone(),
        operation_id: operation.clone(), view_id: view.clone(), owner: fence.owner_epoch(), request_digest: digest.clone(),
        phase: rudder_engine::execution::ExecutionPhase::Checkpointed, supervisor_pid: 42, agent_exit: Some(0), checkpoint: Some(f.checkpoint.clone()) };
    let stop = rudder_engine::execution::StopAndCheckpoint { request_id: uuid(), execution_id: fence.attempt_id.clone(),
        operation_id: operation.clone(), view_id: view.clone(), run_id: fence.run_id.clone(), run_epoch: 1, owner: fence.owner_epoch() };
    let check = |record, stop: &rudder_engine::execution::StopAndCheckpoint| validate_completion_identity(
        &fence, &operation, &view, &digest, 42, &ExecutionRecord::V1(record), stop);
    check(record.clone(), &stop).unwrap();
    for fault in 0..5 {
        let mut changed = record.clone();
        match fault { 0 => changed.owner.epoch += 1, 1 => changed.execution_id = uuid(),
            2 => changed.request_digest = "b".repeat(64), 3 => changed.supervisor_pid += 1,
            _ => changed.checkpoint = None }
        assert!(check(changed, &stop).is_err());
    }
    let mut changed = stop.clone(); changed.run_epoch += 1; assert!(check(record, &changed).is_err());
    // Metadata success above creates no SettledManagedCheckpoint or authority.
}

#[test]
fn managed_acceptance_candidate_preserves_actual_complete_view_topology_environment_and_cursor() {
    let f = Fixture::new(); let candidate = f.candidate();
    let source = snapshot::decode_source(&f.store.read_object(&candidate.source).unwrap()).unwrap();
    let runnable = snapshot::decode_runnable(&f.store.read_object(&candidate.runnable).unwrap()).unwrap();
    assert_eq!(source.directory, f.checkpoint.directory);
    assert_eq!(source.projection_policy, f.policy); assert_eq!(source.consistency, CheckpointConsistency::Quiesced);
    assert_eq!(runnable.view_root, f.checkpoint.directory); assert_eq!(runnable.hardlinks, f.checkpoint.hardlinks);
    assert_eq!(runnable.environment, f.base.environment); assert_eq!(runnable.knowledge_cursor, 7);
    f.store.pin("test-candidate", &candidate.runnable).unwrap();
    assert!(f.store.retained_closure(&candidate.runnable).unwrap().contains(&candidate.source));
}

#[test]
fn managed_acceptance_refuses_unqualified_partition_missing_environment_or_different_policy() {
    let f = Fixture::new();
    let mut base = f.base.clone(); base.environment = None;
    assert!(versioned_candidate(&f.store, &f.stage, &base, &f.checkpoint, &f.policy).is_err());
    let other = f.store.put_blob(b"another policy").unwrap();
    assert!(versioned_candidate(&f.store, &f.stage, &f.base, &f.checkpoint, &other).is_err());
    let mut base = f.base.clone(); base.view_root = f.checkpoint.directory.clone();
    assert!(versioned_candidate(&f.store, &f.stage, &base, &f.checkpoint, &f.policy).is_err());
    let mut profile = WorkspaceProfile::source_only(); profile.scratch = vec![vec![b"scratch".to_vec()]];
    let mut base = f.base.clone(); base.profile = f.store.put_blob(&canonical_json(&profile).unwrap()).unwrap();
    assert!(versioned_candidate(&f.store, &f.stage, &base, &f.checkpoint, &f.policy).is_err());
}

#[test]
fn managed_acceptance_private_integration_never_adopts_public_directory_or_ignores_existing_lock() {
    let f = Fixture::new(); let path = f.directory.join("integration");
    fs::DirBuilder::new().mode(0o755).create(&path).unwrap();
    assert!(PrivateIntegration::open(path.clone(), "Fixture", "fixture@invalid.example").is_err());
    fs::remove_dir(&path).unwrap(); fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    let (_workspace, _repo) = pollster::block_on(Workspace::init_internal_git(&settings("Fixture", "fixture@invalid.example").unwrap(), &path)).unwrap();
    let held = PrivateIntegration::open(path.clone(), "Fixture", "fixture@invalid.example").unwrap();
    assert!(PrivateIntegration::open(path.clone(), "Fixture", "fixture@invalid.example").is_err());
    fs::rename(path.join("rudder-acceptance.lock"), path.join("retained-original.lock")).unwrap();
    File::create(path.join("rudder-acceptance.lock")).unwrap();
    assert!(held.check_current().is_err(), "a replacement pathname cannot replace the original held lock");
}

#[test]
fn managed_acceptance_replaced_repository_or_store_refuses_callbacks_while_outer_lock_is_held() {
    for relative in [".jj/repo", ".jj/repo/store", ".jj/repo/op_store", ".jj/repo/op_heads",
        ".jj/repo/index", ".jj/repo/submodule_store", ".jj/repo/store/git", ".jj/repo/store/git/objects"] {
        let f = Fixture::new(); let path = f.directory.join("integration");
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let (_workspace, _repo) = pollster::block_on(Workspace::init_internal_git(
            &settings("Fixture", "fixture@invalid.example").unwrap(), &path)).unwrap();
        let held = PrivateIntegration::open(path.clone(), "Fixture", "fixture@invalid.example").unwrap();
        let original_lock = held.lock.metadata().unwrap();
        let replaced = path.join(relative);
        fs::rename(&replaced, f.directory.join("original-store-retained")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&replaced).unwrap();
        let current_lock = fs::symlink_metadata(path.join("rudder-acceptance.lock")).unwrap();
        assert_eq!((original_lock.dev(), original_lock.ino()), (current_lock.dev(), current_lock.ino()));
        assert!(held.check_current().is_err(), "{relative} replacement must invalidate the retained owner");
        let dummy = PreparedVcsOperation { format_version: 1,
            record: ObjectRef::from_bytes(ObjectKind::Blob, b"not read after custody rejection"),
            operation_id: "1".repeat(128), expected_operation_heads: vec!["2".repeat(128)],
            commit_id: "3".repeat(40), proof_identity: ObjectRef::from_bytes(ObjectKind::Blob, b"unused") };
        let mut publisher = CheckedJjPublisher { repository: &held, store: &f.store };
        for result in [publisher.inspect(&dummy), publisher.publish(&dummy)] {
            assert!(result.unwrap_err().to_string().contains("directory custody changed"),
                "callback must refuse BEFORE reading the dummy record or attempting publication");
        }
        let still_held = held.lock.metadata().unwrap();
        assert_eq!((still_held.dev(), still_held.ino()), (original_lock.dev(), original_lock.ino()));
    }
}

#[test]
fn managed_acceptance_symlinked_repository_or_external_git_store_never_opens() {
    for relative in [".jj", ".jj/repo", ".jj/repo/store", ".jj/repo/store/git", ".jj/repo/store/git/objects"] {
        let f = Fixture::new(); let path = f.directory.join("integration");
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let (_workspace, _repo) = pollster::block_on(Workspace::init_internal_git(
            &settings("Fixture", "fixture@invalid.example").unwrap(), &path)).unwrap();
        let moved = f.directory.join("external-original");
        fs::rename(path.join(relative), &moved).unwrap();
        std::os::unix::fs::symlink(&moved, path.join(relative)).unwrap();
        assert!(PrivateIntegration::open(path, "Fixture", "fixture@invalid.example").is_err(), "{relative}");
    }
}

struct LoseAck<'a> { inner: CheckedJjPublisher<'a>, publishes: usize }
impl VcsPublisher for LoseAck<'_> {
    fn inspect(&mut self, prepared: &PreparedVcsOperation) -> Result<VcsPublicationObservation> { self.inner.inspect(prepared) }
    fn publish(&mut self, prepared: &PreparedVcsOperation) -> Result<VcsPublicationObservation> {
        self.publishes += 1; ensure!(self.inner.publish(prepared)? == VcsPublicationObservation::Published, "actual publication failed");
        anyhow::bail!("lost ACK after actual private jj publication")
    }
}

#[test]
fn managed_acceptance_real_versioned_projection_and_journal_publish_reconcile_lost_ack_once() {
    for lose_ack in [false, true] {
        let f = Fixture::new(); let path = f.directory.join("integration");
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let (_workspace, repo) = pollster::block_on(Workspace::init_internal_git(&settings("Fixture", "fixture@invalid.example").unwrap(), &path)).unwrap();
        let baseline = repo.store().get_commit(repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();
        let mut tx = repo.start_transaction();
        tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK), RefTarget::normal(baseline.id().clone()));
        let repo = pollster::block_on(tx.commit("initialize controlled integration bookmark")).unwrap();
        let held = PrivateIntegration::open(path, "Fixture", "fixture@invalid.example").unwrap();
        let candidate = f.candidate(); f.store.pin("candidate", &candidate.runnable).unwrap();
        let projected = pollster::block_on(project_source(f.store.clone(), &candidate.directory, &baseline.tree(),
            &f.policy, None, None, ProjectionBudget::default())).unwrap();
        let verified = pollster::block_on(verify_versioned_source(f.store.clone(), &f.stage, &candidate.source,
            &f.policy, &projected.tree, Some(&projected.record), ProjectionBudget::default())).unwrap();
        let mut journal = Journal::open(f.directory.join("journal.sqlite")).unwrap();
        let owner = journal.create_project("project", "owner", &baseline.id().hex(), 1000).unwrap();
        journal.observe_run_attempt(&owner, "run", "attempt", 1000).unwrap();
        let binding = AttemptBinding { format_version: 1, project_id: owner.project_id.clone(),
            owner: rudder_engine::protocol::OwnerEpoch { owner: owner.owner_id.clone(), epoch: owner.epoch },
            run_id: "run".into(), run_epoch: 1, attempt_id: "attempt".into(), view_id: "view".into(),
            seed: f.base.view_root.clone(), baseline: CommitIdentity::from_jj(&baseline),
            baseline_operation: JjOperationId(repo.op_id().hex()), projection_policy: f.policy.clone(), profile: f.base.profile.clone() };
        let evidence = IntegrationEvidence { tested_head: baseline.id().hex(), tested_source_root: candidate.source.clone(),
            tested_environment_root: candidate.environment.clone(), action_results: vec![verified.evidence().clone()],
            checks: serde_json::json!({"controlled_actual_mapping_verified":true}) };
        validate_gate_input(&candidate, &binding, &evidence).unwrap();
        let mut wrong = evidence.clone(); wrong.tested_head = "f".repeat(40);
        assert!(validate_gate_input(&candidate, &binding, &wrong).is_err());
        wrong = evidence.clone(); wrong.tested_source_root = f.base.source.clone();
        assert!(validate_gate_input(&candidate, &binding, &wrong).is_err());
        let request = VcsIntentRequest { idempotency_key: "one-managed-checkpoint".into(),
            source_run: VcsRunFence { run_id: "run".into(), run_epoch: 1, attempt_id: "attempt".into(), view_id: "view".into() },
            expected_head: baseline.id().hex(), proposed_source_root: candidate.source.clone(),
            proposed_environment_root: candidate.environment.clone(), recovery_root: candidate.runnable.clone(),
            seed_root: binding.seed.clone(), projection_policy: f.policy.clone(), profile: candidate.profile.clone(),
            expected_view_root: Some(candidate.directory.clone()), hardlinks: Some(candidate.hardlinks.clone()),
            versions: BTreeMap::new(), evidence };
        let intent = journal.begin_vcs_intent(&owner, &f.store, &request, 1000).unwrap();
        let mut tx = repo.start_transaction();
        let commit = pollster::block_on(tx.repo_mut().new_commit(vec![baseline.id().clone()], projected.tree).write()).unwrap();
        let proof = SealedSourceProof { format_version: 1, binding, recovery_root: candidate.runnable,
            source: candidate.source, environment: Some(candidate.environment), tree: JjTreeIdentity::from_jj(&commit.tree()) };
        let prepared = pollster::block_on(prepare_verified_operation(tx, intent.intent_id.clone(), vec![JjOperationId(repo.op_id().hex())],
            GitCommitId(baseline.id().hex()), CommitIdentity::from_jj(&commit), proof, &verified)).unwrap();
        let envelope = persist_prepared_record(&f.store, &f.stage, &prepared).unwrap();
        journal.attach_vcs_operation(&owner, &f.store, &intent.intent_id, &envelope, 1000).unwrap();
        if lose_ack {
            let mut publisher = LoseAck { inner: CheckedJjPublisher { repository: &held, store: &f.store }, publishes: 0 };
            let receipt = journal.publish_vcs_intent(&owner, &intent.intent_id, &mut publisher, 1000).unwrap();
            assert_eq!(receipt.state, VcsIntentState::Uncertain);
            assert!(journal.publish_vcs_intent(&owner, &intent.intent_id, &mut publisher, 1000).is_err());
            let recovered = journal.reconcile_vcs_intent(&owner, &intent.intent_id, &mut publisher, 1000).unwrap();
            assert_eq!(recovered.state, VcsIntentState::Published); assert!(recovered.integration.is_some());
            assert_eq!(publisher.publishes, 1);
        } else {
            let mut publisher = CheckedJjPublisher { repository: &held, store: &f.store };
            let receipt = journal.publish_vcs_intent(&owner, &intent.intent_id, &mut publisher, 1000).unwrap();
            assert_eq!(receipt.state, VcsIntentState::Published); assert_eq!(receipt.integration.unwrap().new_head, commit.id().hex());
        }
        assert_eq!(journal.project_state(&owner.project_id).unwrap().integration_head, commit.id().hex());
    }
}
