//! Typed metadata fixtures plus an actual bounded UnixStream receiver.
use super::*;
use crate::{
    delta::PartialTreeReceipt,
    execution::{
        network::{
            NetworkBinding, NetworkBudgets, NetworkEpoch, NetworkExecutionProfile, NetworkMode,
            NetworkProfileV1,
        },
        CheckpointArtifactV2, ExecutionIdentityV2, ExecutionRecordV2, ExecutionStateV2,
        QuiescedPartialCheckpointV1,
    },
    protocol::{canonical_json, ObjectRef},
    remote_cache::CacheScope,
    remote_root::{AttemptBinding, RootBinding},
    store::ObjectSetReceipt,
    vfs::{
        rpc::{AttemptContext, ObservationBinding},
        DirtyCursor, ObservationConsistency, PartialCaptureReceipt,
    },
};
use sha2::{Digest, Sha256};
fn identity() -> ExecutionIdentityV2 {
    ExecutionIdentityV2 {
        project_id: "project".into(),
        request_id: Uuid::new_v4().to_string(),
        run_id: "run".into(),
        run_epoch: 3,
        execution_id: "11111111-1111-1111-1111-111111111111".into(),
        operation_id: "22222222-2222-2222-2222-222222222222".into(),
        view_id: "33333333-3333-3333-3333-333333333333".into(),
        owner: crate::protocol::OwnerEpoch {
            owner: "owner".into(),
            epoch: 7,
        },
        request_digest: "a".repeat(64),
        supervisor_pid: std::process::id(),
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
        exclusion_policy_digest: crate::vfs::exclusion_policy_digest(
            &crate::store::ImportPolicy::default(),
        )
        .unwrap(),
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
            exclusion_policy_digest: crate::vfs::exclusion_policy_digest(
                &crate::store::ImportPolicy::default(),
            )
            .unwrap(),
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
fn fixture() -> (ManagedExecutionBinding, ManagedShutdownReceipt) {
    let checkpoint = partial();
    let mut id = identity();
    id.execution_id = checkpoint.capture.attempt.execution_id.clone();
    let binding = ManagedExecutionBinding {
        format_version: 4,
        request_id: id.request_id.clone(),
        run_id: id.run_id.clone(),
        run_epoch: id.run_epoch,
        execution_id: id.execution_id.clone(),
        operation_id: id.operation_id.clone(),
        view_id: id.view_id.clone(),
        owner: id.owner.clone(),
        execution_profile: "offline-command-v1".into(),
        frontend_start: ManagedFrontendStart::Immediate,
        server_command: None,
        guest_socket: "/tmp/cx.sock".into(),
        profile: Some(ExecutionProfileV4 {
            format_version: 1,
            request_id: id.request_id.clone(),
            network: NetworkProfileV1 {
                format_version: 1,
                execution_profile: NetworkExecutionProfile::CloudUid1500LinuxV1,
                binding: NetworkBinding {
                    project_id: id.project_id.clone(),
                    owner: id.owner.owner.clone(),
                    owner_epoch: NetworkEpoch::new(id.owner.epoch).unwrap(),
                    run_id: id.run_id.clone(),
                    run_epoch: NetworkEpoch::new(id.run_epoch).unwrap(),
                    request_id: id.request_id.clone(),
                    execution_id: id.execution_id.clone(),
                    operation_id: id.operation_id.clone(),
                    view_id: id.view_id.clone(),
                    network_id: Uuid::new_v4().to_string(),
                },
                mode: NetworkMode::OfflineV1,
                policy: None,
                policy_sha256: None,
                readiness_nonce: Uuid::new_v4().to_string(),
                budgets: NetworkBudgets {
                    max_connections: 0,
                    max_dns_jobs: 0,
                    max_request_bytes: 0,
                    max_buffer_bytes: 0,
                    max_bytes_per_connection: 0,
                    max_total_bytes: 0,
                    connect_timeout_ms: 0,
                    idle_timeout_ms: 0,
                    total_timeout_ms: 0,
                },
            },
            workspace: ExecutionWorkspaceSelectionV4::RemoteSourceOnly {
                selection: crate::vfs::remote_launch::RemoteLaunchRequest {
                    format_version: 1,
                    request_id: id.request_id.clone(),
                    root: checkpoint.capture.base.clone(),
                    attempt: checkpoint.capture.attempt.clone(),
                    observation: checkpoint.observation.clone(),
                    projection_policy: ObjectRef::from_bytes(ObjectKind::Blob, b"policy"),
                    accepted_source: ObjectRef::from_bytes(ObjectKind::SourceRoot, b"source"),
                    accepted_seed: ObjectRef::from_bytes(ObjectKind::Directory, b"seed"),
                },
                host_context: ProtectedHostContextRef {
                    format_version: 1,
                    path: "/workspace/context".into(),
                    sha256: "f".repeat(64),
                    size_bytes: 1,
                },
            },
        }),
    };
    let receipt = ManagedShutdownReceipt {
        request: checkpoint.stop.clone(),
        kernel_drained: true,
        record: ExecutionRecord::V2(ExecutionRecordV2 {
            format_version: 2,
            identity: id,
            state: ExecutionStateV2::Checkpointed {
                agent_exit: 143,
                checkpoint: CheckpointArtifactV2::PartialRemote(checkpoint),
            },
        }),
    };
    (binding, receipt)
}
#[test]
fn binding4_artifact_requires_full_sealed_partial_family_and_original_budget() {
    let (binding, receipt) = fixture();
    let digest = "a".repeat(64);
    receipt.record.validate_metadata().unwrap();
    validate_shutdown_artifact(&binding, &digest, &receipt).unwrap();
    let budget = FinalizationBudget::begin(Duration::from_secs(2)).unwrap();
    let provisional = source_shutdown::ProvisionalSourceShutdown::received(
        &binding,
        &digest,
        std::process::id(),
        receipt.clone(),
        budget.clone(),
    )
    .unwrap();
    assert_eq!(provisional.budget().constraint(), budget.constraint());
    assert_eq!(provisional.record().agent_exit(), Some(143));
    budget.cancel();
    assert!(provisional.budget().remaining().is_err());
    for case in [
        "request",
        "source",
        "namespace",
        "attempt",
        "local",
        "record1",
    ] {
        let mut changed = binding.clone();
        let mut receipt = receipt.clone();
        match case {
            "request" => changed.request_id = Uuid::new_v4().to_string(),
            "source" => {
                let ExecutionWorkspaceSelectionV4::RemoteSourceOnly { selection, .. } =
                    &mut changed.profile.as_mut().unwrap().workspace
                else {
                    panic!()
                };
                selection.root.root_id.push('x');
            }
            "namespace" => {
                let ExecutionWorkspaceSelectionV4::RemoteSourceOnly { selection, .. } =
                    &mut changed.profile.as_mut().unwrap().workspace
                else {
                    panic!()
                };
                selection.observation.namespace = Uuid::new_v4().to_string();
            }
            "attempt" => changed.run_epoch += 1,
            "local" => {
                changed.profile.as_mut().unwrap().workspace =
                    ExecutionWorkspaceSelectionV4::LocalComplete {}
            }
            "record1" => {
                receipt.record = ExecutionRecord::V1(ExecutionRecordV1 {
                    format_version: 1,
                    execution_id: binding.execution_id.clone(),
                    operation_id: binding.operation_id.clone(),
                    view_id: binding.view_id.clone(),
                    owner: binding.owner.clone(),
                    request_digest: digest.clone(),
                    phase: ExecutionPhase::Checkpointed,
                    supervisor_pid: std::process::id(),
                    agent_exit: Some(0),
                    checkpoint: None,
                })
            }
            _ => unreachable!(),
        }
        assert!(
            validate_shutdown_artifact(&changed, &digest, &receipt).is_err(),
            "{case}"
        );
    }
}
#[test]
fn actual_unix_binding4_receiver_refuses_without_owned_network_readiness() {
    let (binding, receipt) = fixture();
    let digest = "a".repeat(64);
    let (parent, mut helper) = UnixStream::pair().unwrap();
    helper
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (commands, receive) = mpsc::sync_channel(1);
    let (events, results) = mpsc::sync_channel(4);
    let budget = FinalizationBudget::begin(Duration::from_secs(2)).unwrap();
    commands
        .send(QueuedCommand {
            wire: ParentCommand::StopV4 {
                request: receipt.request.clone(),
                constraint: budget.constraint(),
            },
            budget: Some(budget),
        })
        .unwrap();
    let worker = std::thread::spawn(move || {
        managed_client_loop(
            parent,
            &binding,
            &digest,
            std::process::id(),
            &pidfd(std::process::id()).unwrap(),
            receive,
            &events,
            &std::sync::atomic::AtomicBool::new(false),
        )
    });
    let (stop, descriptors): (ParentCommand, _) = receive_control_packet_fds(&mut helper).unwrap();
    assert!(matches!(stop, ParentCommand::StopV4 { .. }));
    assert!(descriptors.is_empty());
    send_control_packet(&mut helper, &ControlEvent::Shutdown { receipt }, None).unwrap();
    let error = worker.join().unwrap().unwrap_err().to_string();
    assert!(
        error.contains("no actual backend network readiness"),
        "{error}"
    );
    assert!(
        results.try_recv().is_err(),
        "neither legacy nor provisional event without actual readiness"
    );
}
