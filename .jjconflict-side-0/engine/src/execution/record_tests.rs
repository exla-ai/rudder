use super::*;
use crate::{
    delta::PartialTreeReceipt,
    protocol::{canonical_json, HashAlgorithm},
    remote_cache::CacheScope,
    remote_root::{AttemptBinding, RootBinding},
    store::ObjectSetReceipt,
    vfs::{rpc::AttemptContext, DirtyCursor},
};
use serde_json::{json, Value};

// Captured independently from the unchanged ten-field Record1 schema.
const LEGACY_GOLDENS: &[(&str, &str, &str, &str)] = &[
    (
        "preparing",
        r#"{"agent_exit":null,"checkpoint":null,"execution_id":"11111111-1111-1111-1111-111111111111","format_version":1,"operation_id":"22222222-2222-2222-2222-222222222222","owner":{"epoch":9007199254740993,"owner":"owner-π"},"phase":"preparing","request_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","supervisor_pid":4242,"view_id":"33333333-3333-3333-3333-333333333333"}"#,
        r#"{"checksum":"d355fc2991a5b46e560df3377841172e46c432524f3d4c51106abaffd06c42cc","record":{"agent_exit":null,"checkpoint":null,"execution_id":"11111111-1111-1111-1111-111111111111","format_version":1,"operation_id":"22222222-2222-2222-2222-222222222222","owner":{"epoch":9007199254740993,"owner":"owner-π"},"phase":"preparing","request_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","supervisor_pid":4242,"view_id":"33333333-3333-3333-3333-333333333333"}}"#,
        "d355fc2991a5b46e560df3377841172e46c432524f3d4c51106abaffd06c42cc",
    ),
    (
        "running",
        r#"{"agent_exit":null,"checkpoint":null,"execution_id":"11111111-1111-1111-1111-111111111111","format_version":1,"operation_id":"22222222-2222-2222-2222-222222222222","owner":{"epoch":9007199254740993,"owner":"owner-π"},"phase":"running","request_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","supervisor_pid":4242,"view_id":"33333333-3333-3333-3333-333333333333"}"#,
        r#"{"checksum":"d4ace330296a1947fa838a286ec129965464a5309104cc0dee41a30c9e02b240","record":{"agent_exit":null,"checkpoint":null,"execution_id":"11111111-1111-1111-1111-111111111111","format_version":1,"operation_id":"22222222-2222-2222-2222-222222222222","owner":{"epoch":9007199254740993,"owner":"owner-π"},"phase":"running","request_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","supervisor_pid":4242,"view_id":"33333333-3333-3333-3333-333333333333"}}"#,
        "d4ace330296a1947fa838a286ec129965464a5309104cc0dee41a30c9e02b240",
    ),
    (
        "orphaned",
        r#"{"agent_exit":null,"checkpoint":null,"execution_id":"11111111-1111-1111-1111-111111111111","format_version":1,"operation_id":"22222222-2222-2222-2222-222222222222","owner":{"epoch":9007199254740993,"owner":"owner-π"},"phase":"orphaned","request_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","supervisor_pid":4242,"view_id":"33333333-3333-3333-3333-333333333333"}"#,
        r#"{"checksum":"93d1f8f79ae78025cfba383f78c5b42e9133e853d0f4ef68c09e141865717137","record":{"agent_exit":null,"checkpoint":null,"execution_id":"11111111-1111-1111-1111-111111111111","format_version":1,"operation_id":"22222222-2222-2222-2222-222222222222","owner":{"epoch":9007199254740993,"owner":"owner-π"},"phase":"orphaned","request_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","supervisor_pid":4242,"view_id":"33333333-3333-3333-3333-333333333333"}}"#,
        "93d1f8f79ae78025cfba383f78c5b42e9133e853d0f4ef68c09e141865717137",
    ),
    (
        "checkpointed",
        r#"{"agent_exit":143,"checkpoint":{"directory":{"algorithm":"sha256","digest":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd","kind":"directory","size_bytes":42},"format_version":1,"hardlinks":{"algorithm":"sha256","digest":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","kind":"blob","size_bytes":7}},"execution_id":"11111111-1111-1111-1111-111111111111","format_version":1,"operation_id":"22222222-2222-2222-2222-222222222222","owner":{"epoch":9007199254740993,"owner":"owner-π"},"phase":"checkpointed","request_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","supervisor_pid":4242,"view_id":"33333333-3333-3333-3333-333333333333"}"#,
        r#"{"checksum":"846292cc7a68248bdb39606d1465d2de04044f85d11b431dcad0bf3778c6f5eb","record":{"agent_exit":143,"checkpoint":{"directory":{"algorithm":"sha256","digest":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd","kind":"directory","size_bytes":42},"format_version":1,"hardlinks":{"algorithm":"sha256","digest":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","kind":"blob","size_bytes":7}},"execution_id":"11111111-1111-1111-1111-111111111111","format_version":1,"operation_id":"22222222-2222-2222-2222-222222222222","owner":{"epoch":9007199254740993,"owner":"owner-π"},"phase":"checkpointed","request_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","supervisor_pid":4242,"view_id":"33333333-3333-3333-3333-333333333333"}}"#,
        "846292cc7a68248bdb39606d1465d2de04044f85d11b431dcad0bf3778c6f5eb",
    ),
];

fn complete() -> VfsCheckpoint {
    VfsCheckpoint {
        format_version: 1,
        directory: ObjectRef {
            kind: ObjectKind::Directory,
            algorithm: HashAlgorithm::Sha256,
            digest: "d".repeat(64),
            size_bytes: 42,
        },
        hardlinks: ObjectRef {
            kind: ObjectKind::Blob,
            algorithm: HashAlgorithm::Sha256,
            digest: "b".repeat(64),
            size_bytes: 7,
        },
    }
}
fn legacy(phase: ExecutionPhase) -> ExecutionRecordV1 {
    ExecutionRecordV1 {
        format_version: 1,
        execution_id: "11111111-1111-1111-1111-111111111111".into(),
        operation_id: "22222222-2222-2222-2222-222222222222".into(),
        view_id: "33333333-3333-3333-3333-333333333333".into(),
        owner: OwnerEpoch {
            owner: "owner-π".into(),
            epoch: 9_007_199_254_740_993,
        },
        request_digest: "a".repeat(64),
        phase,
        supervisor_pid: 4242,
        agent_exit: if phase == ExecutionPhase::Checkpointed {
            Some(143)
        } else {
            None
        },
        checkpoint: if phase == ExecutionPhase::Checkpointed {
            Some(complete())
        } else {
            None
        },
    }
}
fn identity() -> ExecutionIdentityV2 {
    let record = legacy(ExecutionPhase::Preparing);
    ExecutionIdentityV2 {
        project_id: "project".into(),
        request_id: "44444444-4444-4444-4444-444444444444".into(),
        run_id: "run".into(),
        run_epoch: 9_007_199_254_740_995,
        execution_id: record.execution_id,
        operation_id: record.operation_id,
        view_id: record.view_id,
        owner: record.owner,
        request_digest: record.request_digest,
        supervisor_pid: record.supervisor_pid,
    }
}
fn partial() -> QuiescedPartialCheckpointV1 {
    let id = identity();
    let freeze_intent = ObjectRef::from_bytes(ObjectKind::Blob, b"fixture immutable freeze intent");
    let drain_record =
        ObjectRef::from_bytes(ObjectKind::Blob, b"fixture independent drain audit record");
    let mut evidence = vec![freeze_intent.clone(), drain_record.clone()];
    evidence.sort();
    let observation = ObservationBinding {
        format_version: 1,
        attempt: AttemptContext {
            execution_id: id.execution_id.clone(),
            operation_id: id.operation_id.clone(),
        },
        view_id: id.view_id.clone(),
        owner: id.owner.clone(),
        namespace: "55555555-5555-5555-5555-555555555555".into(),
        profile: ObjectRef::from_bytes(ObjectKind::Blob, b"fixture profile"),
        exclusion_policy_digest: "c".repeat(64),
    };
    let cursor = DirtyCursor {
        view_id: id.view_id.clone(),
        owner: id.owner.clone(),
        namespace: observation.namespace.clone(),
        generation: 9_007_199_254_740_997,
    };
    QuiescedPartialCheckpointV1 {
        format_version: 1,
        execution_request_digest: id.request_digest.clone(),
        stop: StopAndCheckpoint {
            request_id: "77777777-7777-7777-7777-777777777777".into(),
            execution_id: id.execution_id.clone(),
            operation_id: id.operation_id.clone(),
            view_id: id.view_id.clone(),
            run_id: id.run_id.clone(),
            run_epoch: id.run_epoch,
            owner: id.owner.clone(),
        },
        observation,
        capture: PartialCaptureReceipt {
            format_version: 1,
            view_id: id.view_id.clone(),
            owner: id.owner.clone(),
            attempt: AttemptBinding {
                project_id: id.project_id.clone(),
                run_id: id.run_id.clone(),
                run_epoch: id.run_epoch,
                owner: id.owner.clone(),
                execution_id: id.execution_id.clone(),
                operation_id: id.operation_id.clone(),
                view_id: id.view_id.clone(),
            },
            base: RootBinding {
                scope: CacheScope {
                    provider_id: "fixture-cas".into(),
                    tenant_id: "fixture-tenant".into(),
                },
                root_id: "fixture-root".into(),
                root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"fixture runnable"),
            },
            begin: cursor.clone(),
            end: cursor,
            exclusion_policy_digest: "c".repeat(64),
            request_nonce: "77777777-7777-7777-7777-777777777777".into(),
            previous_head: None,
            head_sequence: 1,
            retention: PartialTreeReceipt {
                format_version: 1,
                plan: ObjectRef::from_bytes(ObjectKind::Blob, b"fixture delta plan"),
                local: ObjectSetReceipt {
                    format_version: 1,
                    pin_id: "capture:fixture".into(),
                    staging_id: "88888888-8888-8888-8888-888888888888".into(),
                    set_digest: "f".repeat(64),
                    generation: 9_007_199_254_740_999,
                    object_count: 3,
                    total_bytes: 512,
                },
            },
            consistency: ObservationConsistency::LiveBestEffort,
        },
        evidence_retention: ObjectSetReceipt {
            format_version: 1,
            pin_id: "execution:evidence:fixture".into(),
            staging_id: "99999999-9999-9999-9999-999999999999".into(),
            set_digest: hex::encode(Sha256::digest(canonical_json(&evidence).unwrap())),
            generation: 9_007_199_254_741_001,
            object_count: 2,
            total_bytes: freeze_intent.size_bytes + drain_record.size_bytes,
        },
        freeze_intent,
        drain_record,
    }
}
fn v2(state: ExecutionStateV2) -> ExecutionRecord {
    ExecutionRecord::V2(ExecutionRecordV2 {
        format_version: 2,
        identity: identity(),
        state,
    })
}
fn partial_record() -> ExecutionRecord {
    v2(ExecutionStateV2::Checkpointed {
        agent_exit: 0,
        checkpoint: CheckpointArtifactV2::PartialRemote(partial()),
    })
}

#[test]
fn independent_record1_and_envelope_goldens_preserve_every_phase_and_null() {
    for (name, raw, envelope, checksum) in LEGACY_GOLDENS {
        let phase = match *name {
            "preparing" => ExecutionPhase::Preparing,
            "running" => ExecutionPhase::Running,
            "orphaned" => ExecutionPhase::Orphaned,
            "checkpointed" => ExecutionPhase::Checkpointed,
            _ => unreachable!(),
        };
        let old = legacy(phase);
        assert_eq!(canonical_json(&old).unwrap(), raw.as_bytes());
        let record = ExecutionRecord::V1(old);
        assert_eq!(canonical_json(&record).unwrap(), raw.as_bytes());
        assert_eq!(hex::encode(Sha256::digest(raw.as_bytes())), *checksum);
        assert_eq!(encode_envelope(&record).unwrap(), envelope.as_bytes());
        let decoded = decode_envelope(envelope.as_bytes()).unwrap();
        assert!(matches!(decoded, ExecutionRecord::V1(_)));
        assert_eq!(decoded.phase(), phase);
        assert_eq!(decoded.format_version(), 1);
        assert_eq!(canonical_json(&decoded).unwrap(), raw.as_bytes());
        if phase != ExecutionPhase::Checkpointed {
            let value: Value = serde_json::from_str(raw).unwrap();
            assert!(value.get("agent_exit").is_some_and(Value::is_null));
            assert!(value.get("checkpoint").is_some_and(Value::is_null));
        }
    }
}
#[test]
fn numeric_version_dispatch_has_no_missing_alias_or_unknown_fallback() {
    let original: Value = serde_json::from_str(LEGACY_GOLDENS[0].1).unwrap();
    for value in [
        json!(0),
        json!(3),
        json!(4),
        json!("1"),
        json!(1.0),
        json!(null),
        json!(true),
    ] {
        let mut changed = original.clone();
        changed["format_version"] = value;
        assert!(serde_json::from_value::<ExecutionRecord>(changed).is_err());
    }
    let mut missing = original;
    missing.as_object_mut().unwrap().remove("format_version");
    assert!(serde_json::from_value::<ExecutionRecord>(missing).is_err());
    let mut mismatched = legacy(ExecutionPhase::Preparing);
    mismatched.format_version = 2;
    assert!(canonical_json(&ExecutionRecord::V1(mismatched)).is_err());
    let mut changed = serde_json::to_value(v2(ExecutionStateV2::Preparing)).unwrap();
    changed["format_version"] = 1.into();
    assert!(serde_json::from_value::<ExecutionRecord>(changed).is_err());
}
#[test]
fn duplicate_keys_survive_dispatch_and_cannot_hide_behind_valid_legacy_checksum() {
    let raw = LEGACY_GOLDENS[0].1;
    for (needle, replacement) in [
        (
            "\"format_version\":1",
            "\"format_version\":1,\"format_version\":1",
        ),
        (
            "\"epoch\":9007199254740993",
            "\"epoch\":9007199254740993,\"epoch\":9007199254740993",
        ),
        (
            "\"checkpoint\":null",
            "\"checkpoint\":null,\"checkpoint\":null",
        ),
    ] {
        let duplicate = raw.replacen(needle, replacement, 1);
        assert!(serde_json::from_str::<ExecutionRecord>(&duplicate).is_err());
        let envelope = format!(
            r#"{{"record":{duplicate},"checksum":"{}"}}"#,
            LEGACY_GOLDENS[0].3
        );
        assert!(decode_envelope(envelope.as_bytes()).is_err());
    }
    let duplicate = format!(
        r#"{{"record":{raw},"record":{raw},"checksum":"{}"}}"#,
        LEGACY_GOLDENS[0].3
    );
    assert!(decode_envelope(duplicate.as_bytes()).is_err());
}
#[test]
fn record2_unit_states_reject_residual_fields_and_terminal_shape_is_required() {
    for phase in ["preparing", "running", "orphaned"] {
        for field in ["extra", "checkpoint", "agent_exit"] {
            let mut value = serde_json::to_value(v2(ExecutionStateV2::Preparing)).unwrap();
            let mut state = json!({"phase":phase});
            state[field] = Value::Null;
            value["state"] = state;
            assert!(
                serde_json::from_value::<ExecutionRecord>(value).is_err(),
                "{phase}.{field}"
            );
        }
    }
    let original = serde_json::to_value(partial_record()).unwrap();
    for field in ["agent_exit", "checkpoint"] {
        let mut value = original.clone();
        value["state"].as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<ExecutionRecord>(value).is_err());
    }
    for (pointer, value) in [
        ("/state/phase", json!("paused")),
        ("/state/checkpoint/kind", json!("complete")),
        ("/state/checkpoint/value", Value::Null),
        ("/state/checkpoint/extra", json!(true)),
    ] {
        let mut changed = original.clone();
        if pointer.ends_with("/extra") {
            changed["state"]["checkpoint"]["extra"] = value;
        } else {
            *changed.pointer_mut(pointer).unwrap() = value;
        }
        assert!(serde_json::from_value::<ExecutionRecord>(changed).is_err());
    }
}
#[test]
fn all_record2_states_and_checkpoint_kinds_round_trip_as_audit_data() {
    for state in [
        ExecutionStateV2::Preparing,
        ExecutionStateV2::Running,
        ExecutionStateV2::Orphaned,
        ExecutionStateV2::Checkpointed {
            agent_exit: 0,
            checkpoint: CheckpointArtifactV2::CompleteLocal(complete()),
        },
        ExecutionStateV2::Checkpointed {
            agent_exit: 143,
            checkpoint: CheckpointArtifactV2::PartialRemote(partial()),
        },
    ] {
        let record = v2(state);
        let bytes = canonical_json(&record).unwrap();
        let envelope = encode_envelope(&record).unwrap();
        let decoded = decode_envelope(&envelope).unwrap();
        assert_eq!(canonical_json(&decoded).unwrap(), bytes);
        assert_eq!(decoded.format_version(), 2);
        assert_eq!(decoded.owner().epoch, 9_007_199_254_740_993);
    }
    let record = partial_record();
    let CheckpointRef::PartialRemote(artifact) = record.checkpointed_data().unwrap() else {
        panic!("partial must stay partial");
    };
    assert_eq!(
        artifact.capture.consistency,
        ObservationConsistency::LiveBestEffort
    );
    assert_eq!(
        artifact.capture.retention.local.generation,
        9_007_199_254_740_999
    );
    assert_eq!(
        artifact.evidence_retention.generation,
        9_007_199_254_741_001
    );
    let value = serde_json::to_value(&record).unwrap();
    assert!(serde_json::from_value::<VfsCheckpoint>(value["state"]["checkpoint"].clone()).is_err());
    assert!(
        serde_json::from_value::<VfsCheckpoint>(value["state"]["checkpoint"]["value"].clone())
            .is_err()
    );
    assert!(serde_json::from_value::<ExecutionRecordV1>(value).is_err());
}
#[test]
fn partial_audit_binding_mismatches_and_fake_consistency_are_rejected() {
    let original = serde_json::to_value(partial_record()).unwrap();
    let prefix = "/state/checkpoint/value";
    for (path, value) in [
        ("/execution_request_digest", json!("b".repeat(64))),
        ("/capture/attempt/project_id", json!("other-project")),
        ("/capture/owner/epoch", json!(3)),
        ("/capture/begin/generation", json!(1)),
        ("/capture/exclusion_policy_digest", json!("d".repeat(64))),
        ("/capture/consistency", json!("quiesced")),
        ("/capture/retention/plan/kind", json!("directory")),
        ("/capture/retention/local/generation", json!(0)),
        ("/stop/run_epoch", json!(1)),
        (
            "/observation/attempt/operation_id",
            json!("99999999-9999-9999-9999-999999999999"),
        ),
        ("/freeze_intent/kind", json!("runnable_root")),
        ("/drain_record/digest", json!("A".repeat(64))),
        (
            "/capture/request_nonce",
            json!("66666666-6666-6666-6666-666666666666"),
        ),
        ("/evidence_retention/format_version", json!(2)),
        ("/evidence_retention/object_count", json!(1)),
        ("/evidence_retention/total_bytes", json!(0)),
        ("/evidence_retention/set_digest", json!("0".repeat(64))),
        ("/evidence_retention/generation", json!(0)),
        ("/evidence_retention/generation", json!(u64::MAX)),
    ] {
        let mut changed = original.clone();
        *changed.pointer_mut(&format!("{prefix}{path}")).unwrap() = value;
        assert!(
            serde_json::from_value::<ExecutionRecord>(changed).is_err(),
            "{path}"
        );
    }
    let mut changed = original;
    changed["state"]["checkpoint"]["value"]["kernel_drained"] = json!(true);
    assert!(serde_json::from_value::<ExecutionRecord>(changed).is_err());
}
#[test]
fn strict_record2_duplicate_tags_nested_keys_and_unknown_identity_fields_reject() {
    let raw = String::from_utf8(canonical_json(&partial_record()).unwrap()).unwrap();
    for (needle, duplicate) in [
        (
            "\"phase\":\"checkpointed\"",
            "\"phase\":\"checkpointed\",\"phase\":\"checkpointed\"",
        ),
        (
            "\"kind\":\"partial_remote\"",
            "\"kind\":\"partial_remote\",\"kind\":\"partial_remote\"",
        ),
        (
            "\"epoch\":9007199254740993",
            "\"epoch\":9007199254740993,\"epoch\":9007199254740993",
        ),
        (
            "\"generation\":9007199254741001",
            "\"generation\":9007199254741001,\"generation\":9007199254741001",
        ),
    ] {
        assert!(
            serde_json::from_str::<ExecutionRecord>(&raw.replacen(needle, duplicate, 1)).is_err()
        );
    }
    let mut value = serde_json::from_str::<Value>(&raw).unwrap();
    value["identity"]["extra"] = json!(true);
    assert!(serde_json::from_value::<ExecutionRecord>(value).is_err());
}
#[test]
fn record_and_envelope_bounds_use_valid_json_and_checksum_changes_fail() {
    let small =
        String::from_utf8(canonical_json(&v2(ExecutionStateV2::Preparing)).unwrap()).unwrap();
    let oversized = format!("{{{}{}", " ".repeat(MAX_RECORD_V2_BYTES), &small[1..]);
    assert!(serde_json::from_str::<Value>(&oversized).is_ok());
    assert!(serde_json::from_str::<ExecutionRecord>(&oversized).is_err());
    let envelope = LEGACY_GOLDENS[0].2;
    let changed = envelope.replacen("\"supervisor_pid\":4242", "\"supervisor_pid\":4243", 1);
    assert!(decode_envelope(changed.as_bytes()).is_err());
    let mut oversized = envelope.as_bytes().to_vec();
    oversized.resize(MAX_EXECUTION_RECORD_BYTES + 1, b' ');
    assert!(serde_json::from_slice::<Value>(&oversized).is_ok());
    assert!(decode_envelope(&oversized).is_err());
}
#[test]
fn incomplete_legacy_checkpoint_is_historical_data_not_terminal_confirmation() {
    let mut record = legacy(ExecutionPhase::Checkpointed);
    record.agent_exit = None;
    let decoded = decode_envelope(&encode_envelope(&ExecutionRecord::V1(record)).unwrap()).unwrap();
    assert_eq!(decoded.phase(), ExecutionPhase::Checkpointed);
    assert!(decoded.checkpointed_data().is_err());
}
