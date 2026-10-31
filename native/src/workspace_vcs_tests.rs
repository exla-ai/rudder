use super::*;
use rudder_engine::execution::{BrokerBinding, ReadOnlyRuntime, WorkspaceObservationProfile};
use rudder_engine::protocol::{ObjectKind, ObjectRef, OwnerEpoch};
use rudder_engine::vfs::rpc::AttemptContext;
use rudder_engine::workspace::WorkspaceProfile;
use rudder_vcs_bridge::{
    CommitIdentity, GitCommitId, GitTreeId, JjChangeId, JjOperationId, JjTreeIdentity,
};
use std::os::unix::fs::{symlink, PermissionsExt};

struct Fixture {
    root: PathBuf,
    spec: ExecutionSpec,
    context: WorkspaceVcsContext,
}
impl Fixture {
    fn new() -> Self {
        let root = Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("nvcs-{}", uuid::Uuid::new_v4().simple()));
        private_directory(&root, true).unwrap();
        for name in [
            "private",
            "cas",
            "vfs",
            "projections",
            "state",
            "context",
            "endpoints",
            "agent",
        ] {
            private_directory(&root.join(name), true).unwrap();
        }
        fs::create_dir_all(root.join("private/.git")).unwrap();
        let execution_id = uuid::Uuid::new_v4().to_string();
        let owner = OwnerEpoch {
            owner: "owner".into(),
            epoch: 1,
        };
        let profile = WorkspaceProfile::source_only();
        let policy =
            vfs_provider::SourceProjectionPolicy::from_import_policy(&profile.source_exclusions())
                .unwrap();
        let profile_ref =
            ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&profile).unwrap());
        let observation = ObservationBinding {
            format_version: 1,
            attempt: AttemptContext {
                execution_id: execution_id.clone(),
                operation_id: uuid::Uuid::new_v4().to_string(),
            },
            view_id: uuid::Uuid::new_v4().to_string(),
            owner: owner.clone(),
            namespace: uuid::Uuid::new_v4().to_string(),
            profile: profile_ref.clone(),
            exclusion_policy_digest: rudder_engine::vfs::exclusion_policy_digest(
                &profile.source_exclusions(),
            )
            .unwrap(),
        };
        let attempt = AttemptBinding {
            format_version: 1,
            project_id: "p".into(),
            owner: owner.clone(),
            run_id: "r".into(),
            run_epoch: 1,
            attempt_id: execution_id.clone(),
            view_id: observation.view_id.clone(),
            seed: ObjectRef::from_bytes(ObjectKind::Directory, b"source fixture"),
            baseline: CommitIdentity {
                commit: GitCommitId("1".repeat(40)),
                change: JjChangeId("2".repeat(32)),
                parents: vec![GitCommitId("3".repeat(40))],
                tree: JjTreeIdentity {
                    terms: vec![GitTreeId("4".repeat(40))],
                    labels: vec![],
                },
            },
            baseline_operation: JjOperationId("5".repeat(128)),
            projection_policy: policy.reference().unwrap(),
            profile: profile_ref,
        };
        let binary = Path::new("/usr/bin/true").canonicalize().unwrap();
        let binary_bytes = fs::read(&binary).unwrap();
        let context = WorkspaceVcsContext {
            format_version: 1,
            kind: KIND.into(),
            service: CommandServiceConfig {
                format_version: 1,
                view: rudder_vcs_bridge::rpc_view::RpcViewConfig {
                    format_version: 1,
                    attempt,
                    observation: observation.clone(),
                    socket: root.join("workspace/workspace.sock"),
                    identity_file: root.join("workspace/identity.json"),
                    private_repository: root.join("private"),
                    private_git: root.join("private/.git"),
                    projection_state: root.join("projections"),
                    cas_root: root.join("cas"),
                    policy,
                    budget: rudder_vcs_bridge::projection_io::ProjectionBudget::default(),
                },
                binary: binary.clone(),
                state_directory: root.join("state"),
                git_config_sha256: "a".repeat(64),
                user_name: "Rudder Test".into(),
                user_email: "fixture@invalid.example".into(),
                command_timeout_ms: 20_000,
            },
            executable: ExecutableIdentity {
                sha256: hex_bytes(Sha256::digest(&binary_bytes)),
                size_bytes: binary_bytes.len() as u64,
            },
            guest_executable: "/opt/runtime/rudder-jj".into(),
        };
        let reference = write_host_context(&root.join("context/launch.json"), &context).unwrap();
        let identity = rudder_engine::agent_api::AgentIdentityFile {
            format_version: 1,
            project_id: "p".into(),
            run_id: "r".into(),
            run_epoch: 1,
            owner_epoch: 1,
            token: "fixture-capability-not-valid".into(),
        };
        fs::write(
            root.join("agent/identity.json"),
            canonical_json(&identity).unwrap(),
        )
        .unwrap();
        fs::set_permissions(
            root.join("agent/identity.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let spec = ExecutionSpec {
            profile: None,
            format_version: 3,
            execution_id,
            operation_id: observation.attempt.operation_id.clone(),
            execution_root: root.join("executions"),
            manager_root: root.join("vfs"),
            store_root: root.join("cas"),
            authority_roots: vec![
                root.join("private"),
                root.join("projections"),
                root.join("state"),
                root.join("context"),
            ],
            view_id: observation.view_id.clone(),
            owner,
            command: vec!["/usr/bin/true".into()],
            environment: BTreeMap::new(),
            runtime: vec![ReadOnlyRuntime {
                source: binary,
                guest_path: "/opt/runtime/rudder-jj".into(),
            }],
            home_files: vec![],
            broker_directory: Some(root.join("agent")),
            broker_binding: Some(BrokerBinding {
                project_id: "p".into(),
                run_id: "r".into(),
                run_epoch: 1,
                owner_epoch: 1,
            }),
            workspace_observation: Some(WorkspaceObservationProfile {
                binding: observation,
                endpoint_directory: root.join("workspace"),
                application: Some(HelperApplicationSpec {
                    format_version: 1,
                    kind: KIND.into(),
                    host_context: reference,
                    projection_directory: root.join("endpoints/vcs"),
                }),
            }),
            timeout_seconds: 30,
        };
        Self {
            root,
            spec,
            context,
        }
    }
    fn application(&self) -> &HelperApplicationSpec {
        self.spec
            .workspace_observation
            .as_ref()
            .unwrap()
            .application
            .as_ref()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn sealed_context_is_immutable_canonical_hash_bound_and_private() {
    let f = Fixture::new();
    read_context(&f.application().host_context).unwrap();
    assert_eq!(
        write_host_context(&f.application().host_context.path, &f.context).unwrap(),
        f.application().host_context
    );
    let mut other = f.context.clone();
    other.service.user_name = "changed".into();
    assert!(write_host_context(&f.application().host_context.path, &other).is_err());
    fs::set_permissions(
        &f.application().host_context.path,
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(read_context(&f.application().host_context).is_err());
    fs::set_permissions(
        &f.application().host_context.path,
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut value = serde_json::to_value(&f.context).unwrap();
    value["unknown"] = true.into();
    let bytes = canonical_json(&value).unwrap();
    fs::write(&f.application().host_context.path, &bytes).unwrap();
    let reference = ProtectedHostContextRef {
        size_bytes: bytes.len() as u64,
        sha256: hex_bytes(Sha256::digest(&bytes)),
        ..f.application().host_context.clone()
    };
    assert!(read_context(&reference).is_err());
}
#[test]
fn protected_context_rejects_symlink_components_hardlinks_and_substituted_bytes() {
    let f = Fixture::new();
    let path = &f.application().host_context.path;
    let link = f.root.join("alias");
    symlink(path.parent().unwrap(), &link).unwrap();
    let reference = ProtectedHostContextRef {
        path: link.join("launch.json"),
        ..f.application().host_context.clone()
    };
    assert!(read_context(&reference).is_err());
    fs::hard_link(path, f.root.join("hardlink.json")).unwrap();
    assert!(read_context(&f.application().host_context).is_err());
    fs::remove_file(f.root.join("hardlink.json")).unwrap();
    fs::write(path, b"{}\n").unwrap();
    assert!(read_context(&f.application().host_context).is_err());
}
#[test]
fn application_context_cannot_substitute_run_scope_cas_or_guest_runtime() {
    let f = Fixture::new();
    validate_assignment(&f.context, &f.spec, f.application()).unwrap();
    let mut value = f.context.clone();
    value.service.view.attempt.run_epoch += 1;
    assert!(validate_assignment(&value, &f.spec, f.application()).is_err());
    let mut value = f.context.clone();
    value.service.view.cas_root = f.root.join("other-cas");
    assert!(validate_assignment(&value, &f.spec, f.application()).is_err());
    let mut value = f.context.clone();
    value.guest_executable = "/usr/bin/another".into();
    assert!(validate_assignment(&value, &f.spec, f.application()).is_err());
    let mut value = f.context.clone();
    value.service.state_directory = value.service.view.private_repository.join("commands");
    assert!(validate_assignment(&value, &f.spec, f.application()).is_err());
}
#[cfg(target_os = "linux")]
#[test]
fn held_elf_descriptor_executes_exact_inode_after_image_path_replacement() {
    let mut f = Fixture::new();
    let path = f.root.join("managed-jj");
    fs::copy("/usr/bin/true", &path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o555)).unwrap();
    f.context.service.binary = path.clone();
    let mut pinned = pin_executable(&f.context).unwrap();
    let replacement = f.root.join("replacement");
    fs::copy("/usr/bin/false", &replacement).unwrap();
    fs::rename(replacement, &path).unwrap();
    assert_eq!(
        digest(&mut pinned, MAX_EXECUTABLE).unwrap().0,
        f.context.executable.sha256
    );
    assert!(
        std::process::Command::new(format!("/proc/self/fd/{}", pinned.as_raw_fd()))
            .status()
            .unwrap()
            .success()
    );
    assert!(!std::process::Command::new(&path)
        .status()
        .unwrap()
        .success());
}

#[cfg(target_os = "linux")]
#[test]
fn companion_resolution_requires_exact_sibling_and_never_uses_existing_service_binary() {
    let f = Fixture::new();
    assert!(context_for_executable(
        &f.root.join("rudder-native"),
        f.context.service.clone(),
        f.context.guest_executable.clone()
    )
    .is_err());
    fs::copy("/usr/bin/true", f.root.join("jj")).unwrap();
    assert!(context_for_executable(
        &f.root.join("rudder-native"),
        f.context.service.clone(),
        f.context.guest_executable.clone()
    )
    .is_err());
    let companion = f.root.join("rudder-jj");
    fs::copy("/usr/bin/true", &companion).unwrap();
    fs::set_permissions(&companion, fs::Permissions::from_mode(0o555)).unwrap();
    let context = context_for_executable(
        &f.root.join("rudder-native"),
        f.context.service.clone(),
        f.context.guest_executable.clone(),
    )
    .unwrap();
    assert_eq!(context.service.binary, companion);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn native_factory_refuses_non_linux_before_broker_or_application_side_effects() {
    let mut f = Fixture::new();
    let projection = f.application().projection_directory.clone();
    let broker = f.root.join("uncreated-broker");
    f.spec.broker_directory = Some(broker.clone());
    for observation_only in [false, true] {
        if observation_only {
            f.spec.format_version = 2;
            f.spec.workspace_observation.as_mut().unwrap().application = None;
        }
        f.spec.validate().unwrap();
        let error = match services(&f.spec) {
            Err(error) => error,
            Ok(_) => panic!("non-Linux factory must refuse workspace services"),
        };
        assert_eq!(
            error.downcast_ref::<std::io::Error>().map(|error| error.kind()),
            Some(std::io::ErrorKind::Unsupported)
        );
        assert!(!broker.exists());
        assert!(!projection.exists());
        assert!(!f.context.service.view.private_repository.join(".jj").exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_factory_uses_real_broker_scope_and_never_prepares_wc_before_rpc_ready() {
    use rudder_engine::journal::{CapabilityGrant, Journal};
    use std::collections::BTreeSet;
    let mut f = Fixture::new();
    f.spec.validate().unwrap();
    let mut journal = Journal::open(f.root.join("authority.sqlite")).unwrap();
    let owner = journal.create_project("p", "owner", "base", 0).unwrap();
    journal
        .observe_run_attempt(&owner, "r", &f.spec.execution_id, 0)
        .unwrap();
    let grant = CapabilityGrant {
        principal: "agent:r".into(),
        run_id: "r".into(),
        run_epoch: 1,
        operations: BTreeSet::from(["workspace:observe".into()]),
        recipients: BTreeMap::new(),
        scopes: BTreeSet::from([rudder_engine::execution::workspace_scope(
            &f.context.service.view.observation,
        )
        .unwrap()]),
        topics: BTreeSet::new(),
        expires_at_ms: None,
    };
    let token = journal.issue_capability(&owner, &grant, 0).unwrap();
    let identity = rudder_engine::agent_api::AgentIdentityFile {
        format_version: 1,
        project_id: "p".into(),
        run_id: "r".into(),
        run_epoch: 1,
        owner_epoch: 1,
        token: token.clone(),
    };
    let socket = crate::agent_broker::socket_path(&f.root, "p", "r", &f.spec.execution_id);
    let identity_file = crate::agent_broker::project_identity(&socket, &identity).unwrap();
    let broker = crate::agent_broker::AgentBroker::spawn(crate::agent_broker::BrokerConfig {
        journal_path: f.root.join("authority.sqlite"),
        identity_file,
        socket_path: socket.clone(),
    })
    .unwrap();
    f.spec.broker_directory = Some(socket.parent().unwrap().into());
    let mut services = services(&f.spec).unwrap();
    let rudder_engine::execution::HelperWorkspaceServices::Complete { authority, adoption_witness } = &services.workspace else {
        panic!("VCS service requires the complete workspace protocol");
    };
    let authority = authority.clone();
    assert!(adoption_witness.is_some() && services.application.is_some());
    assert!(!f
        .context
        .service
        .view
        .private_repository
        .join(".jj")
        .exists());
    assert!(!f.application().projection_directory.exists());
    let mut wrong = f
        .spec
        .workspace_observation
        .as_ref()
        .unwrap()
        .binding
        .clone();
    wrong.namespace = uuid::Uuid::new_v4().to_string();
    assert!(authority.validate(&wrong).is_err());
    let application = f.application();
    let ready = WorkspaceRpcReady {
        binding: &f.context.service.view.observation,
        socket: Path::new("/not-the-owning-socket"),
        identity_file: &f.context.service.view.identity_file,
        application,
    };
    assert!(services
        .application
        .as_mut()
        .unwrap()
        .start(&ready)
        .is_err());
    journal.revoke_capability(&owner, &token, 1).unwrap();
    assert!(authority
        .validate(&f.context.service.view.observation)
        .is_err());
    broker.close();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!socket.exists());
}
