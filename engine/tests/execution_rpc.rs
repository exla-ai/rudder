//! Contract tests require no host namespace/mount/device changes. The explicit
//! standalone driver separately qualifies the actual restricted guest on Fly.
#![cfg(target_os = "linux")]
use rudder_engine::{
    execution::{self, BrokerBinding, ExecutionSpec, WorkspaceObservationProfile},
    protocol::{canonical_json, ObjectKind, ObjectRef, OwnerEpoch},
    vfs::rpc::{AttemptContext, ObservationBinding},
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use uuid::Uuid;

fn spec(path: &Path) -> ExecutionSpec {
    let broker = path.join("agent");
    fs::create_dir(&broker).unwrap();
    fs::set_permissions(&broker, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(broker.join("identity.json"), br#"{"format_version":1,"project_id":"p","run_id":"r","run_epoch":1,"owner_epoch":3,"token":"fixture-token-never-authorized"}"#).unwrap();
    fs::set_permissions(
        broker.join("identity.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let execution_id = Uuid::new_v4().to_string();
    let operation_id = Uuid::new_v4().to_string();
    let view_id = Uuid::new_v4().to_string();
    let owner = OwnerEpoch {
        owner: "execution-rpc-test".into(),
        epoch: 3,
    };
    let binding = ObservationBinding {
        format_version: 1,
        attempt: AttemptContext {
            execution_id: execution_id.clone(),
            operation_id: operation_id.clone(),
        },
        view_id: view_id.clone(),
        owner: owner.clone(),
        namespace: Uuid::new_v4().to_string(),
        profile: ObjectRef::from_bytes(
            ObjectKind::Blob,
            b"fixture profile is validated again by actual owning view",
        ),
        exclusion_policy_digest: "a".repeat(64),
    };
    ExecutionSpec {
        profile: None,
        format_version: 2,
        execution_id,
        operation_id,
        view_id,
        owner,
        execution_root: path.join("executions"),
        manager_root: path.join("manager"),
        store_root: path.join("cas"),
        authority_roots: vec![path.join("authority")],
        command: vec!["/usr/bin/true".into()],
        environment: BTreeMap::new(),
        runtime: vec![],
        home_files: vec![],
        broker_directory: Some(broker),
        broker_binding: Some(BrokerBinding {
            project_id: "p".into(),
            run_id: "r".into(),
            run_epoch: 1,
            owner_epoch: 3,
        }),
        workspace_observation: Some(WorkspaceObservationProfile {
            binding,
            endpoint_directory: path.join("workspace"),
            application: None,
        }),
        timeout_seconds: 30,
    }
}
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("erpc-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}
#[test]
fn legacy_canonical_request_bytes_are_preserved_and_profile_versions_fail_closed() {
    let temp = fixture();
    let mut value = spec(temp.path());
    value.validate().unwrap();
    let v2 = value.clone();
    value.format_version = 1;
    assert!(value.validate().is_err());
    value.workspace_observation = None;
    value.validate().unwrap();
    let bytes = canonical_json(&value).unwrap();
    let legacy = serde_json::to_value(&value).unwrap();
    assert!(legacy.get("workspace_observation").is_none());
    let parsed: ExecutionSpec = serde_json::from_value(legacy).unwrap();
    assert_eq!(canonical_json(&parsed).unwrap(), bytes);
    value.format_version = 2;
    assert!(
        value.validate().is_err(),
        "v2 must never silently omit observations"
    );
    value = v2;
    let legacy_v2 = serde_json::to_value(&value).unwrap();
    assert!(legacy_v2["workspace_observation"]
        .get("application")
        .is_none());
    let roundtrip: ExecutionSpec = serde_json::from_value(legacy_v2).unwrap();
    assert_eq!(
        canonical_json(&roundtrip).unwrap(),
        canonical_json(&value).unwrap()
    );
    value.format_version = 3;
    assert!(value.validate().is_err());
}

fn profiled_spec(path: &Path) -> ExecutionSpec {
    use execution::{ExecutionProfileV4, ExecutionWorkspaceSelectionV4};
    let mut value = spec(path);
    let request_id = Uuid::new_v4().to_string();
    let network: execution::network::NetworkProfileV1 = serde_json::from_value(serde_json::json!({
        "format_version":1,"execution_profile":"cloud_uid1500_linux_v1",
        "binding":{"project_id":"p","owner":value.owner.owner,"owner_epoch":"3","run_id":"r","run_epoch":"1",
            "request_id":request_id,"execution_id":value.execution_id,"operation_id":value.operation_id,
            "view_id":value.view_id,"network_id":Uuid::new_v4().to_string()},
        "mode":"offline_v1","policy":null,"policy_sha256":null,
        "budgets":{"max_connections":0,"max_dns_jobs":0,"max_request_bytes":0,"max_buffer_bytes":0,
            "max_bytes_per_connection":0,"max_total_bytes":0,"connect_timeout_ms":0,"idle_timeout_ms":0,"total_timeout_ms":0},
        "readiness_nonce":Uuid::new_v4().to_string()
    })).unwrap();
    value.format_version = 4;
    value.profile = Some(ExecutionProfileV4 {
        format_version: 1,
        request_id: request_id.clone(),
        network,
        workspace: ExecutionWorkspaceSelectionV4::LocalComplete {},
    });
    value
}

fn managed_binding(value: &ExecutionSpec) -> execution::ManagedExecutionBinding {
    execution::ManagedExecutionBinding {
        format_version: 3,
        request_id: value.profile.as_ref().unwrap().request_id.clone(),
        run_id: "r".into(),
        run_epoch: 1,
        execution_id: value.execution_id.clone(),
        operation_id: value.operation_id.clone(),
        view_id: value.view_id.clone(),
        owner: value.owner.clone(),
        execution_profile: "offline-command-v1".into(),
        frontend_start: execution::ManagedFrontendStart::Immediate,
        server_command: None,
        guest_socket: "/tmp/cx.sock".into(),
        profile: value.profile.clone(),
    }
}

#[test]
fn explicit_v4_offline_profile_binds_all_ids_and_cannot_fall_back_to_legacy_control() {
    use execution::ExecutionWorkspaceSelectionV4;
    let temp = fixture();
    let value = profiled_spec(temp.path());
    value.validate().unwrap();
    let binding = managed_binding(&value);
    binding.validate(&value).unwrap();
    let mut old = binding.clone();
    old.format_version = 2;
    assert!(old.validate(&value).is_err());
    for kind in ["epoch", "request", "connected", "environment", "legacy"] {
        let mut changed = value.clone();
        match kind {
            "epoch" => {
                changed.profile.as_mut().unwrap().network.binding.run_epoch =
                    execution::network::NetworkEpoch::new(2).unwrap()
            }
            "request" => changed.profile.as_mut().unwrap().request_id = Uuid::new_v4().to_string(),
            "connected" => {
                changed.profile.as_mut().unwrap().network.mode =
                    execution::network::NetworkMode::QualificationHttpsConnectV1
            }
            "environment" => {
                changed.environment.insert(
                    "RUDDER_PARTIAL_WORKSPACE_SOCKET".into(),
                    "/tmp/forged.sock".into(),
                );
            }
            _ => changed.format_version = 3,
        }
        assert!(
            changed.validate().is_err(),
            "{kind} must not reuse the sealed v4 grant"
        );
    }
    assert!(serde_json::from_str::<ExecutionWorkspaceSelectionV4>(
        r#"{"kind":"local_complete","extra":true}"#
    )
    .is_err());
    let invoked = Arc::new(AtomicBool::new(false));
    let counter = invoked.clone();
    assert!(execution::run_helper_with_services(
        value,
        Box::new(move |_| {
            counter.store(true, Ordering::Release);
            anyhow::bail!("must never construct services without v4 managed control")
        })
    )
    .is_err());
    assert!(!invoked.load(Ordering::Acquire));
}

#[test]
fn protected_host_context_rejects_fifo_symlink_and_changed_bytes() {
    use execution::ProtectedHostContextRef;
    use sha2::{Digest, Sha256};
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::symlink},
    };
    let temp = fixture();
    let path = temp.path().join("context.json");
    let bytes = br#"{"format_version":1}"#;
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let reference = ProtectedHostContextRef {
        format_version: 1,
        path: path.clone(),
        sha256: hex::encode(Sha256::digest(bytes)),
        size_bytes: bytes.len() as u64,
    };
    assert_eq!(reference.read_bytes().unwrap(), bytes);
    let linked = temp.path().join("linked");
    symlink(&path, &linked).unwrap();
    assert!(ProtectedHostContextRef {
        path: linked,
        ..reference.clone()
    }
    .read_bytes()
    .is_err());
    let fifo = temp.path().join("fifo");
    let raw = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o600) }, 0);
    assert!(ProtectedHostContextRef {
        path: fifo,
        ..reference.clone()
    }
    .read_bytes()
    .is_err());
    fs::write(&path, b"changed bytes").unwrap();
    assert!(reference.read_bytes().is_err());
}

#[test]
fn v3_application_is_explicit_hash_bound_and_cannot_shadow_authority_projection() {
    use execution::{
        GuestServiceProjection, HelperApplicationSpec, ProtectedHostContextRef,
        ServiceProjectionEntry, ServiceProjectionKind,
    };
    let temp = fixture();
    let mut value = spec(temp.path());
    let authority = temp.path().join("authority");
    fs::create_dir(&authority).unwrap();
    fs::set_permissions(&authority, fs::Permissions::from_mode(0o700)).unwrap();
    let application = HelperApplicationSpec {
        format_version: 1,
        kind: "fixture-v1".into(),
        host_context: ProtectedHostContextRef {
            format_version: 1,
            path: authority.join("context.json"),
            sha256: "a".repeat(64),
            size_bytes: 2,
        },
        projection_directory: temp.path().join("application"),
    };
    value.workspace_observation.as_mut().unwrap().application = Some(application.clone());
    assert!(
        value.validate().is_err(),
        "v2 cannot silently gain an application"
    );
    value.format_version = 3;
    value.validate().unwrap();
    let projection = GuestServiceProjection {
        directory: application.projection_directory.clone(),
        entries: vec![
            ServiceProjectionEntry {
                name: "client.json".into(),
                node: ServiceProjectionKind::File {
                    max_size_bytes: 32 * 1024,
                },
            },
            ServiceProjectionEntry {
                name: "service.sock".into(),
                node: ServiceProjectionKind::Socket,
            },
        ],
        environment: BTreeMap::from([(
            "RUDDER_APP_CLIENT".into(),
            format!("{}/client.json", application.guest_directory()),
        )]),
    };
    projection.validate(&value).unwrap();
    let mut bad = projection.clone();
    bad.environment
        .insert("RUDDER_AGENT_IDENTITY".into(), "/foreign".into());
    assert!(bad.validate(&value).is_err());
    let mut bad = projection.clone();
    bad.entries[0].name = "../context.json".into();
    assert!(bad.validate(&value).is_err());
    let mut bad = projection.clone();
    bad.entries.push(bad.entries[0].clone());
    assert!(bad.validate(&value).is_err());
    let mut bad = projection;
    bad.directory = authority.clone();
    assert!(bad.validate(&value).is_err());
    value
        .workspace_observation
        .as_mut()
        .unwrap()
        .application
        .as_mut()
        .unwrap()
        .projection_directory = authority;
    assert!(
        value.validate().is_err(),
        "application cannot expose authority files"
    );
}
#[test]
fn assignment_and_exact_capability_scope_bind_every_observation_identity_field() {
    let temp = fixture();
    let value = spec(temp.path());
    let original =
        execution::workspace_scope(&value.workspace_observation.as_ref().unwrap().binding).unwrap();
    for field in [
        "execution",
        "operation",
        "view",
        "owner",
        "namespace",
        "profile",
        "policy",
    ] {
        let mut changed = value.clone();
        let binding = &mut changed.workspace_observation.as_mut().unwrap().binding;
        match field {
            "execution" => binding.attempt.execution_id = Uuid::new_v4().to_string(),
            "operation" => binding.attempt.operation_id = Uuid::new_v4().to_string(),
            "view" => binding.view_id = Uuid::new_v4().to_string(),
            "owner" => binding.owner.epoch += 1,
            "namespace" => binding.namespace = Uuid::new_v4().to_string(),
            "profile" => {
                binding.profile = ObjectRef::from_bytes(ObjectKind::Blob, b"different profile")
            }
            _ => binding.exclusion_policy_digest = "b".repeat(64),
        }
        assert_ne!(
            execution::workspace_scope(binding).unwrap(),
            original,
            "scope ignored {field}"
        );
        if ["execution", "operation", "view", "owner"].contains(&field) {
            assert!(changed.validate().is_err());
        }
    }
    let mut unscoped = value.clone();
    unscoped.broker_binding = None;
    unscoped.broker_directory = None;
    assert!(unscoped.validate().is_err());
    assert!(
        execution::run_helper(value).is_err(),
        "v2 cannot use a constant-authority legacy entry"
    );
}
#[test]
fn private_endpoint_paths_cannot_overlap_runtime_authority_or_other_projections() {
    let temp = fixture();
    let value = spec(temp.path());
    value.validate().unwrap();
    for path in [
        value.store_root.join("endpoint"),
        value.manager_root.clone(),
        value.authority_roots[0].join("endpoint"),
        value.broker_directory.clone().unwrap(),
        temp.path().to_path_buf(),
    ] {
        let mut changed = value.clone();
        changed
            .workspace_observation
            .as_mut()
            .unwrap()
            .endpoint_directory = path;
        assert!(changed.validate().is_err());
    }
    let mut public = value.clone();
    public
        .workspace_observation
        .as_mut()
        .unwrap()
        .endpoint_directory = "/tmp/public-workspace-rpc".into();
    assert!(public.validate().is_err());
    let mut long = value.clone();
    long.workspace_observation
        .as_mut()
        .unwrap()
        .endpoint_directory = temp.path().join("x".repeat(100));
    assert!(long.validate().is_err());
}
#[test]
fn authority_factory_cannot_run_before_fresh_namespace_admission() {
    let temp = fixture();
    let value = spec(temp.path());
    let invoked = Arc::new(AtomicBool::new(false));
    let observed = invoked.clone();
    // The test harness is multithreaded (and usually not UID1500); either
    // precondition must reject before namespace entry or factory invocation.
    assert!(execution::run_helper_with_services(
        value,
        Box::new(move |_| {
            observed.store(true, Ordering::Release);
            anyhow::bail!("a factory ran before fresh namespace admission")
        })
    )
    .is_err());
    assert!(!invoked.load(Ordering::Acquire));
}

fn remote_spec(path: &Path) -> ExecutionSpec {
    use execution::{ExecutionWorkspaceSelectionV4, ProtectedHostContextRef};
    use rudder_engine::{
        remote_cache::CacheScope,
        remote_root::{AttemptBinding, RootBinding},
        store::ImportPolicy,
        vfs::{exclusion_policy_digest, remote_launch::RemoteLaunchRequest},
    };
    let mut value = profiled_spec(path);
    value.format_version = 5;
    let observation = value.workspace_observation.as_mut().unwrap();
    observation.binding.exclusion_policy_digest =
        exclusion_policy_digest(&ImportPolicy::default()).unwrap();
    let selection = RemoteLaunchRequest {
        format_version: 1,
        request_id: value.profile.as_ref().unwrap().request_id.clone(),
        root: RootBinding {
            scope: CacheScope {
                provider_id: "codec-fixture".into(),
                tenant_id: "tenant".into(),
            },
            root_id: "root".into(),
            root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"metadata only"),
        },
        attempt: AttemptBinding {
            project_id: "p".into(),
            run_id: "r".into(),
            run_epoch: 1,
            owner: value.owner.clone(),
            execution_id: value.execution_id.clone(),
            operation_id: value.operation_id.clone(),
            view_id: value.view_id.clone(),
        },
        observation: observation.binding.clone(),
        projection_policy: ObjectRef::from_bytes(ObjectKind::Blob, b"policy"),
        accepted_source: ObjectRef::from_bytes(ObjectKind::SourceRoot, b"source"),
        accepted_seed: ObjectRef::from_bytes(ObjectKind::Directory, b"seed"),
    };
    let authority = path.join("authority");
    fs::create_dir(&authority).unwrap();
    let context = authority.join("context.json");
    fs::write(&context, b"{}").unwrap();
    value.profile.as_mut().unwrap().workspace = ExecutionWorkspaceSelectionV4::RemoteSourceOnly {
        selection,
        host_context: ProtectedHostContextRef {
            format_version: 1,
            path: context,
            sha256: ObjectRef::from_bytes(ObjectKind::Blob, b"{}").digest,
            size_bytes: 2,
        },
    };
    value
}

#[test]
fn spec5_binding4_are_remote_offline_metadata_and_never_reach_a_factory() {
    use execution::{ExecutionWorkspaceSelectionV4, ManagedFrontendStart};
    let temp = fixture();
    let value = remote_spec(temp.path());
    value.validate().unwrap();
    let mut binding = managed_binding(&value);
    binding.format_version = 4;
    binding.validate(&value).unwrap();
    for spec_version in 1..=5 {
        for binding_version in 2..=4 {
            let mut s = value.clone();
            let mut b = binding.clone();
            s.format_version = spec_version;
            b.format_version = binding_version;
            assert_eq!(
                b.validate(&s).is_ok(),
                matches!((spec_version, binding_version), (4, 3) | (5, 4))
            );
        }
    }
    for pointer in [
        "/profile",
        "/profile/workspace",
        "/profile/workspace/selection",
        "/profile/workspace/host_context",
        "/profile/network",
    ] {
        let mut wire = serde_json::to_value(&value).unwrap();
        wire.pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), true.into());
        assert!(
            serde_json::from_value::<ExecutionSpec>(wire).is_err(),
            "{pointer}"
        );
    }
    let mut wire = serde_json::to_value(&binding).unwrap();
    wire["unknown"] = true.into();
    assert!(serde_json::from_value::<execution::ManagedExecutionBinding>(wire).is_err());
    for kind in ["profile", "local", "connected", "context", "application"] {
        let mut s = value.clone();
        match kind {
            "profile" => s.profile = None,
            "local" => {
                s.profile.as_mut().unwrap().workspace =
                    ExecutionWorkspaceSelectionV4::LocalComplete {}
            }
            "connected" => {
                s.profile.as_mut().unwrap().network.mode =
                    execution::network::NetworkMode::QualificationHttpsConnectV1
            }
            "context" => {
                if let ExecutionWorkspaceSelectionV4::RemoteSourceOnly { host_context, .. } =
                    &mut s.profile.as_mut().unwrap().workspace
                {
                    host_context.format_version = 2;
                }
            }
            _ => {
                s.workspace_observation.as_mut().unwrap().application =
                    Some(execution::HelperApplicationSpec {
                        format_version: 1,
                        kind: "vcs".into(),
                        host_context: match &s.profile.as_ref().unwrap().workspace {
                            ExecutionWorkspaceSelectionV4::RemoteSourceOnly {
                                host_context,
                                ..
                            } => host_context.clone(),
                            _ => unreachable!(),
                        },
                        projection_directory: temp.path().join("vcs"),
                    })
            }
        }
        assert!(s.validate().is_err(), "{kind}");
    }
    // Mutate every shared assignment identity on the remote side alone.
    for pointer in [
        "/request_id",
        "/attempt/project_id",
        "/attempt/run_id",
        "/attempt/run_epoch",
        "/attempt/owner/owner",
        "/attempt/owner/epoch",
        "/attempt/execution_id",
        "/attempt/operation_id",
        "/attempt/view_id",
        "/observation/profile/digest",
    ] {
        let mut wire = serde_json::to_value(&value).unwrap();
        let slot = wire
            .pointer_mut(&format!("/profile/workspace/selection{pointer}"))
            .unwrap();
        *slot = if slot.is_number() {
            99.into()
        } else if pointer.ends_with("digest") {
            "b".repeat(64).into()
        } else {
            Uuid::new_v4().to_string().into()
        };
        let s: ExecutionSpec = serde_json::from_value(wire).unwrap();
        assert!(s.validate().is_err(), "{pointer}");
    }
    for kind in [
        "profile",
        "different-profile",
        "request",
        "server",
        "start",
        "backend",
        "codex",
    ] {
        let mut b = binding.clone();
        match kind {
            "profile" => b.profile = None,
            "different-profile" => {
                b.profile.as_mut().unwrap().network.readiness_nonce = Uuid::new_v4().to_string()
            }
            "request" => b.request_id = Uuid::new_v4().to_string(),
            "server" => b.server_command = Some(vec!["/bin/true".into()]),
            "start" => b.frontend_start = ManagedFrontendStart::AfterHostApiKeyV1,
            "codex" => {
                b.execution_profile = "codex-app-server".into();
                b.frontend_start = ManagedFrontendStart::AfterHostApiKeyV1;
                b.server_command = Some(vec!["/bin/true".into()]);
            }
            _ => b.execution_profile = "claude-sdk".into(),
        }
        assert!(b.validate(&value).is_err(), "{kind}");
    }
    let invoked = Arc::new(AtomicBool::new(false));
    let counter = invoked.clone();
    let result = execution::run_helper_with_services(
        value,
        Box::new(move |_| {
            counter.store(true, Ordering::Release);
            anyhow::bail!("Spec5 must never invoke services")
        }),
    );
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("Spec5 was admitted"),
    };
    assert!(error
        .to_string()
        .contains("specification5 awaits owned source"));
    assert!(!invoked.load(Ordering::Acquire));
}

#[test]
fn old_spec_and_binding_canonical_bytes_match_frozen_schema_snapshots() {
    use execution::{HelperApplicationSpec, ProtectedHostContextRef};
    use sha2::{Digest, Sha256};
    fn normalized(value: &mut serde_json::Value, root: &str) {
        match value {
            serde_json::Value::String(text) => {
                if Uuid::parse_str(text).is_ok() {
                    *text = Uuid::nil().to_string();
                } else if text.starts_with(root) {
                    *text = text.replacen(root, "/fixture", 1);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    normalized(item, root);
                }
            }
            serde_json::Value::Object(items) => {
                for item in items.values_mut() {
                    normalized(item, root);
                }
            }
            _ => {}
        }
    }
    let temp = fixture();
    let v4 = profiled_spec(temp.path());
    let mut snapshots = Vec::new();
    for version in 1..=4 {
        let mut s = v4.clone();
        s.format_version = version;
        if version < 4 {
            s.profile = None;
        }
        if version == 1 {
            s.workspace_observation = None;
        }
        if version == 3 {
            s.workspace_observation.as_mut().unwrap().application = Some(HelperApplicationSpec {
                format_version: 1,
                kind: "fixture-v1".into(),
                host_context: ProtectedHostContextRef {
                    format_version: 1,
                    path: temp.path().join("authority/context.json"),
                    sha256: "a".repeat(64),
                    size_bytes: 2,
                },
                projection_directory: temp.path().join("application"),
            });
        }
        s.validate().unwrap();
        let bytes = canonical_json(&s).unwrap();
        let parsed: ExecutionSpec = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(canonical_json(&parsed).unwrap(), bytes);
        snapshots.push(serde_json::to_value(s).unwrap());
    }
    for version in 2..=3 {
        let mut b = managed_binding(&v4);
        b.format_version = version;
        if version == 2 {
            b.profile = None;
            b.execution_profile = "claude-sdk".into();
        }
        let bytes = canonical_json(&b).unwrap();
        let parsed: execution::ManagedExecutionBinding = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(canonical_json(&parsed).unwrap(), bytes);
        snapshots.push(serde_json::to_value(b).unwrap());
    }
    // SHA256 of the original Spec1..4 and Binding2..3 fixture encodings,
    // with only random UUIDs and the temporary root normalized for stability.
    let expected = [
        "fbb141d4c3a2a24d7c72863666b98f5ef9e4cb66fb920f471d97bcf017487cf4",
        "6144d01f51fbfb3fee830983a0513d927b749cd31fe54f38ac1d466772b4cc9f",
        "cee28f21592a94e13702ffe39edbe817bb5a07176e3ea11b9cf3887008be028c",
        "17c857c36085df16978106986c248c9a202b008d19678c6efe7b9d6f5f91b12e",
        "a6074028749b2ddda3555b9b52ddec973d5a887d7c1dc264d63139a61a16dc46",
        "d48f6879536d074c6df77a34c7b6a5806b89f19cd6bf622894a34f39a52bdfed",
    ];
    for (mut wire, expected) in snapshots.into_iter().zip(expected) {
        normalized(&mut wire, temp.path().to_str().unwrap());
        assert_eq!(
            hex::encode(Sha256::digest(canonical_json(&wire).unwrap())),
            expected
        );
    }
}
