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
