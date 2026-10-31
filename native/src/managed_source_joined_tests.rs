use rudder_engine::{execution::{CheckpointArtifactV2, ExecutionIdentityV2, ExecutionPhase,
    ExecutionRecordV1, ExecutionRecordV2, ExecutionStateV2}, vfs::VfsCheckpoint};
use std::os::unix::fs::symlink;

fn checkpoint_record(f: &SourceFixture) -> ExecutionRecord {
    let fence = &f.assignment.fence;
    let q = f.inputs.q.clone();
    let record = ExecutionRecord::V2(ExecutionRecordV2 { format_version: 2,
        identity: ExecutionIdentityV2 { project_id: fence.owner.project_id.clone(),
            request_id: fence.request_id.clone(), run_id: fence.run_id.clone(), run_epoch: fence.run_epoch,
            execution_id: fence.attempt_id.clone(), operation_id: q.capture.attempt.operation_id.clone(),
            view_id: q.capture.attempt.view_id.clone(), owner: fence.owner_epoch(),
            request_digest: q.execution_request_digest.clone(), supervisor_pid: 4242 },
        state: ExecutionStateV2::Checkpointed { agent_exit: 0, checkpoint: CheckpointArtifactV2::PartialRemote(q) } });
    record.checkpointed_data().unwrap();
    record
}
fn joined_path(f: &SourceFixture) -> PathBuf {
    f.location.attempt_directory(&f.assignment.fence.attempt_id).unwrap().join(JOINED_FILE)
}

#[test]
fn source_joined_checkpoint_replays_exact_original_data_before_publication() {
    let f = fixture(); let fence = &f.assignment.fence; let record = checkpoint_record(&f);
    assert!(f.location.persist_source_joined_checkpoint(fence, &record).is_err(), "assignment2 must precede joined DATA");
    f.location.persist_source_assignment(&f.assignment).unwrap();
    assert!(f.location.source_joined_checkpoint(&fence.attempt_id).unwrap().is_none());
    f.location.persist_source_joined_checkpoint(fence, &record).unwrap();
    // A lost local response may only be reconciled by exact DATA re-read/replay.
    let restored = f.location.source_joined_checkpoint(&fence.attempt_id).unwrap().unwrap();
    assert_eq!(canonical_json(&restored).unwrap(), canonical_json(&record).unwrap());
    f.location.persist_source_joined_checkpoint(fence, &record).unwrap();
    let saved: SourceJoinedCheckpointData = read_source_record(&joined_path(&f)).unwrap().unwrap();
    assert_eq!(saved.format_version, 1); assert!(saved.data_only); assert_eq!(&saved.fence, fence);
    assert_eq!(saved.record.agent_exit(), Some(0));
    assert!(f.location.source_checkpoint_inputs(&fence.attempt_id).unwrap().is_none(),
        "joined DATA must not synthesize publication intent or ACK");

    let mut conflicting = record.clone();
    let ExecutionRecord::V2(value) = &mut conflicting else { unreachable!() };
    let ExecutionStateV2::Checkpointed { agent_exit, .. } = &mut value.state else { unreachable!() };
    *agent_exit = 7;
    conflicting.checkpointed_data().unwrap();
    assert!(f.location.persist_source_joined_checkpoint(fence, &conflicting).is_err());
    assert_eq!(f.location.source_joined_checkpoint(&fence.attempt_id).unwrap().unwrap().agent_exit(), Some(0));
}

#[test]
fn source_joined_checkpoint_rejects_every_foreign_reserved_fence() {
    let f = fixture(); f.location.persist_source_assignment(&f.assignment).unwrap();
    let record = checkpoint_record(&f);
    for change in 0..7 {
        let mut fence = f.assignment.fence.clone();
        match change {
            0 => fence.owner.project_id = "other-project".into(),
            1 => fence.owner.owner_id = "other-owner".into(),
            2 => fence.owner.epoch += 1,
            3 => fence.run_id = "other-run".into(),
            4 => fence.run_epoch += 1,
            5 => fence.attempt_id = uuid::Uuid::new_v4().to_string(),
            _ => fence.request_id = uuid::Uuid::new_v4().to_string(),
        }
        assert!(f.location.persist_source_joined_checkpoint(&fence, &record).is_err());
    }
    assert!(!joined_path(&f).exists());
}

#[test]
fn source_joined_checkpoint_checks_valid_record_identity_source_and_observation_against_assignment() {
    let f = fixture(); f.location.persist_source_assignment(&f.assignment).unwrap();
    for change in 0..11 {
        let mut record = checkpoint_record(&f);
        let ExecutionRecord::V2(value) = &mut record else { unreachable!() };
        let identity = &mut value.identity;
        let ExecutionStateV2::Checkpointed { checkpoint: CheckpointArtifactV2::PartialRemote(q), .. } = &mut value.state
            else { unreachable!() };
        match change {
            0 => identity.request_id = uuid::Uuid::new_v4().to_string(),
            1 => { identity.project_id = "other-project".into(); q.capture.attempt.project_id = identity.project_id.clone(); }
            2 => { identity.run_id = "other-run".into(); q.capture.attempt.run_id = identity.run_id.clone();
                q.stop.run_id = identity.run_id.clone(); }
            3 => { identity.run_epoch += 1; q.capture.attempt.run_epoch = identity.run_epoch; q.stop.run_epoch = identity.run_epoch; }
            4 => {
                identity.owner.epoch += 1; let owner = identity.owner.clone();
                q.capture.attempt.owner = owner.clone(); q.capture.owner = owner.clone();
                q.capture.begin.owner = owner.clone(); q.capture.end.owner = owner.clone();
                q.observation.owner = owner.clone(); q.stop.owner = owner;
            }
            5 => {
                identity.execution_id = uuid::Uuid::new_v4().to_string();
                q.capture.attempt.execution_id = identity.execution_id.clone();
                q.observation.attempt.execution_id = identity.execution_id.clone(); q.stop.execution_id = identity.execution_id.clone();
            }
            6 => {
                identity.operation_id = uuid::Uuid::new_v4().to_string();
                q.capture.attempt.operation_id = identity.operation_id.clone();
                q.observation.attempt.operation_id = identity.operation_id.clone(); q.stop.operation_id = identity.operation_id.clone();
            }
            7 => {
                identity.view_id = uuid::Uuid::new_v4().to_string(); let view = identity.view_id.clone();
                q.capture.attempt.view_id = view.clone(); q.capture.view_id = view.clone();
                q.capture.begin.view_id = view.clone(); q.capture.end.view_id = view.clone();
                q.observation.view_id = view.clone(); q.stop.view_id = view;
            }
            8 => q.capture.base.root_id = "other-base".into(),
            9 => q.capture.base.root = ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"other source root"),
            _ => q.observation.profile = ObjectRef::from_bytes(ObjectKind::Blob, b"other observation profile"),
        }
        record.checkpointed_data().unwrap(); // Internally valid, so assignment equality must reject it.
        assert!(f.location.persist_source_joined_checkpoint(&f.assignment.fence, &record).is_err(), "foreign binding {change}");
    }
    assert!(!joined_path(&f).exists());
}

#[test]
fn source_joined_checkpoint_refuses_v1_complete_and_nonterminal_records() {
    let f = fixture(); f.location.persist_source_assignment(&f.assignment).unwrap();
    let record = checkpoint_record(&f);
    let ExecutionRecord::V2(value) = &record else { unreachable!() };
    let complete = VfsCheckpoint { format_version: 1,
        directory: ObjectRef::from_bytes(ObjectKind::Directory, b"directory"),
        hardlinks: ObjectRef::from_bytes(ObjectKind::Blob, b"hardlinks") };
    let legacy = ExecutionRecord::V1(ExecutionRecordV1 { format_version: 1,
        execution_id: value.identity.execution_id.clone(), operation_id: value.identity.operation_id.clone(),
        view_id: value.identity.view_id.clone(), owner: value.identity.owner.clone(),
        request_digest: value.identity.request_digest.clone(), supervisor_pid: value.identity.supervisor_pid,
        phase: ExecutionPhase::Checkpointed, agent_exit: Some(0), checkpoint: Some(complete.clone()) });
    legacy.checkpointed_data().unwrap();
    assert!(f.location.persist_source_joined_checkpoint(&f.assignment.fence, &legacy).is_err());
    let mut local = value.clone();
    local.state = ExecutionStateV2::Checkpointed { agent_exit: 0, checkpoint: CheckpointArtifactV2::CompleteLocal(complete) };
    let local = ExecutionRecord::V2(local); local.checkpointed_data().unwrap();
    assert!(f.location.persist_source_joined_checkpoint(&f.assignment.fence, &local).is_err());
    let mut running = value.clone(); running.state = ExecutionStateV2::Running;
    assert!(f.location.persist_source_joined_checkpoint(&f.assignment.fence, &ExecutionRecord::V2(running)).is_err());
    assert!(!joined_path(&f).exists());
}

#[test]
fn source_joined_checkpoint_read_rechecks_envelope_binding_and_canonical_bytes() {
    let f = fixture(); f.location.persist_source_assignment(&f.assignment).unwrap();
    let record = checkpoint_record(&f);
    let original = SourceJoinedCheckpointData { format_version: 1, data_only: true,
        fence: f.assignment.fence.clone(), record };
    let bytes = canonical_json(&original).unwrap();
    for change in 0..5 {
        let mut value = serde_json::to_value(&original).unwrap();
        match change {
            0 => value["format_version"] = 2.into(),
            1 => value["data_only"] = false.into(),
            2 => value["fence"]["request_id"] = uuid::Uuid::new_v4().to_string().into(),
            3 => value["record"]["identity"]["request_id"] = uuid::Uuid::new_v4().to_string().into(),
            _ => value["extra"] = true.into(),
        }
        fs::write(joined_path(&f), canonical_json(&value).unwrap()).unwrap();
        fs::set_permissions(joined_path(&f), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(f.location.source_joined_checkpoint(&f.assignment.fence.attempt_id).is_err());
    }
    fs::write(joined_path(&f), [b"\n".as_slice(), bytes.as_slice()].concat()).unwrap();
    assert!(f.location.source_joined_checkpoint(&f.assignment.fence.attempt_id).is_err());
    fs::write(joined_path(&f), bytes).unwrap();
    assert!(f.location.source_joined_checkpoint(&f.assignment.fence.attempt_id).unwrap().is_some());
}

#[test]
fn source_joined_checkpoint_refuses_symlink_and_oversized_saved_data_without_repair() {
    let f = fixture(); f.location.persist_source_assignment(&f.assignment).unwrap();
    let record = checkpoint_record(&f); let path = joined_path(&f);
    let sentinel = path.with_file_name("sentinel.json"); fs::write(&sentinel, b"kept").unwrap();
    symlink(&sentinel, &path).unwrap();
    assert!(f.location.source_joined_checkpoint(&f.assignment.fence.attempt_id).is_err());
    assert!(f.location.persist_source_joined_checkpoint(&f.assignment.fence, &record).is_err());
    assert_eq!(fs::read(&sentinel).unwrap(), b"kept");
    fs::remove_file(&path).unwrap();
    fs::write(&path, vec![b' '; SOURCE_RECORD_LIMIT + 1]).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(f.location.source_joined_checkpoint(&f.assignment.fence.attempt_id).is_err());
    assert!(f.location.persist_source_joined_checkpoint(&f.assignment.fence, &record).is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), (SOURCE_RECORD_LIMIT + 1) as u64);
}
