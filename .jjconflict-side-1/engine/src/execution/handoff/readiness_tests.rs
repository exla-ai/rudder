//! Metadata admission never substitutes for owned kernel descriptors.
use super::*;
use crate::{
    execution::network::{
        NetworkBinding, NetworkBudgets, NetworkEpoch, NetworkExecutionProfile, NetworkMode,
        NetworkProfileV1,
    },
    protocol::{canonical_json, ObjectRef},
    remote_cache::CacheScope,
    remote_root::{AttemptBinding, RootBinding},
    vfs::{
        remote_launch::RemoteLaunchRequest,
        rpc::{AttemptContext, ObservationBinding},
    },
};
use sha2::{Digest, Sha256};

pub(super) fn binding() -> ManagedExecutionBinding {
    let mut binding = ManagedExecutionBinding {
        format_version: 4,
        request_id: Uuid::new_v4().to_string(),
        run_id: "run".into(),
        run_epoch: 3,
        execution_id: Uuid::new_v4().to_string(),
        operation_id: Uuid::new_v4().to_string(),
        view_id: Uuid::new_v4().to_string(),
        owner: crate::protocol::OwnerEpoch {
            owner: "owner".into(),
            epoch: 7,
        },
        execution_profile: "offline-command-v1".into(),
        frontend_start: ManagedFrontendStart::Immediate,
        server_command: None,
        guest_socket: "/tmp/cx.sock".into(),
        profile: None,
    };
    let selection = RemoteLaunchRequest {
        format_version: 1,
        request_id: binding.request_id.clone(),
        root: RootBinding {
            scope: CacheScope {
                provider_id: "fixture-cas".into(),
                tenant_id: "fixture-tenant".into(),
            },
            root_id: "fixture-root".into(),
            root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"root"),
        },
        attempt: AttemptBinding {
            project_id: "project".into(),
            run_id: binding.run_id.clone(),
            run_epoch: binding.run_epoch,
            owner: binding.owner.clone(),
            execution_id: binding.execution_id.clone(),
            operation_id: binding.operation_id.clone(),
            view_id: binding.view_id.clone(),
        },
        observation: ObservationBinding {
            format_version: 1,
            attempt: AttemptContext {
                execution_id: binding.execution_id.clone(),
                operation_id: binding.operation_id.clone(),
            },
            view_id: binding.view_id.clone(),
            owner: binding.owner.clone(),
            namespace: Uuid::new_v4().to_string(),
            profile: ObjectRef::from_bytes(ObjectKind::Blob, b"profile"),
            exclusion_policy_digest: crate::vfs::exclusion_policy_digest(
                &crate::store::ImportPolicy::default(),
            )
            .unwrap(),
        },
        projection_policy: ObjectRef::from_bytes(ObjectKind::Blob, b"policy"),
        accepted_source: ObjectRef::from_bytes(ObjectKind::SourceRoot, b"source"),
        accepted_seed: ObjectRef::from_bytes(ObjectKind::Directory, b"seed"),
    };
    binding.profile = Some(ExecutionProfileV4 {
        format_version: 1,
        startup: Some(super::super::startup_budget::StartupConstraint::begin(
            super::super::startup_budget::STARTUP_POLICY_LIMIT).unwrap()),
        request_id: binding.request_id.clone(),
        network: NetworkProfileV1 {
            format_version: 1,
            execution_profile: NetworkExecutionProfile::CloudUid1500LinuxV1,
            binding: NetworkBinding {
                project_id: "project".into(),
                owner: binding.owner.owner.clone(),
                owner_epoch: NetworkEpoch::new(binding.owner.epoch).unwrap(),
                run_id: binding.run_id.clone(),
                run_epoch: NetworkEpoch::new(binding.run_epoch).unwrap(),
                request_id: binding.request_id.clone(),
                execution_id: binding.execution_id.clone(),
                operation_id: binding.operation_id.clone(),
                view_id: binding.view_id.clone(),
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
            selection,
            host_context: ProtectedHostContextRef {
                format_version: 1,
                path: "/workspace/context".into(),
                sha256: "f".repeat(64),
                size_bytes: 1,
            },
        },
    });
    binding
}
fn frame(binding: &ManagedExecutionBinding) -> super::super::network_phase::NetworkReadyFrame {
    let network = &binding.profile.as_ref().unwrap().network;
    super::super::network_phase::NetworkReadyFrame {
        format_version: 1,
        execution_id: binding.execution_id.clone(),
        child_nonce: Uuid::new_v4().to_string(),
        execution_request_digest: "a".repeat(64),
        network_profile_digest: hex::encode(Sha256::digest(serde_json::to_vec(network).unwrap())),
        network_id: network.binding.network_id.clone(),
        readiness_nonce: network.readiness_nonce.clone(),
    }
}
fn event(binding: &ManagedExecutionBinding, family: u32) -> ControlEvent {
    let digest = binding
        .profile
        .as_ref()
        .unwrap()
        .partial_binding()
        .unwrap()
        .unwrap()
        .digest()
        .unwrap();
    match family {
        3 => ControlEvent::BackendReadyV3 {
            protocol_version: 3,
            frontend_start: binding.frontend_start,
            execution_id: binding.execution_id.clone(),
            operation_id: binding.operation_id.clone(),
            view_id: binding.view_id.clone(),
            network: frame(binding),
            partial_binding_digest: Some(digest),
        },
        4 => ControlEvent::BackendReadyV4 {
            protocol_version: 4,
            frontend_start: binding.frontend_start,
            execution_id: binding.execution_id.clone(),
            operation_id: binding.operation_id.clone(),
            view_id: binding.view_id.clone(),
            network: frame(binding),
            partial_binding_digest: digest,
        },
        _ => unreachable!(),
    }
}
#[test]
fn readiness4_is_a_strict_distinct_wire_family_and_preserves_v3_shape() {
    let mut binding = binding();
    for family in [3, 4] {
        binding.format_version = family;
        let same_family =
            receive_network_backend(event(&binding, family), vec![], &binding, &"a".repeat(64));
        assert!(same_family
            .err()
            .unwrap()
            .to_string()
            .contains("requires pidfd"));
        let bytes = canonical_json(&event(&binding, family)).unwrap();
        let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(wire["event"], format!("backend_ready_v{family}"));
        assert_eq!(wire["protocol_version"], family);
        let decoded: ControlEvent = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(canonical_json(&decoded).unwrap(), bytes);
        let other = if family == 3 { 4 } else { 3 };
        assert!(
            receive_network_backend(event(&binding, other), vec![], &binding, &"a".repeat(64))
                .err()
                .unwrap()
                .to_string()
                .contains("protocol family")
        );
    }
    let bytes = String::from_utf8(canonical_json(&event(&binding, 4)).unwrap()).unwrap();
    for bad in [
        bytes.replace("\"protocol_version\":4", "\"protocol_version\":3"),
        bytes.replace(
            "\"event\":\"backend_ready_v4\"",
            "\"event\":\"backend_ready_v4\",\"extra\":true",
        ),
        bytes.replace(
            "\"protocol_version\":4",
            "\"protocol_version\":4,\"protocol_version\":4",
        ),
    ] {
        let result = serde_json::from_str::<ControlEvent>(&bad);
        assert!(
            result.is_err()
                || receive_network_backend(result.unwrap(), vec![], &binding, &"a".repeat(64))
                    .is_err()
        );
    }
    let mut wire: serde_json::Value = serde_json::from_str(&bytes).unwrap();
    wire["partial_binding_digest"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<ControlEvent>(wire.clone()).is_err());
    wire.as_object_mut()
        .unwrap()
        .remove("partial_binding_digest");
    assert!(serde_json::from_value::<ControlEvent>(wire).is_err());
}
#[test]
fn readiness4_checks_the_full_offline_source_assignment() {
    let binding = binding();
    validate_network_frame(&binding, &"a".repeat(64), &frame(&binding)).unwrap();
    for case in [
        "request",
        "run",
        "epoch",
        "owner",
        "operation",
        "view",
        "local",
        "frontend",
        "server",
        "profile",
        "network_request",
        "network_project",
    ] {
        let mut changed = binding.clone();
        match case {
            "request" => changed.request_id = Uuid::new_v4().to_string(),
            "run" => changed.run_id.push('x'),
            "epoch" => changed.run_epoch += 1,
            "owner" => changed.owner.epoch += 1,
            "operation" => changed.operation_id = Uuid::new_v4().to_string(),
            "view" => changed.view_id = Uuid::new_v4().to_string(),
            "local" => {
                changed.profile.as_mut().unwrap().workspace =
                    ExecutionWorkspaceSelectionV4::LocalComplete {}
            }
            "frontend" => changed.frontend_start = ManagedFrontendStart::AfterHostApiKeyV1,
            "server" => changed.server_command = Some(vec!["server".into()]),
            "profile" => changed.execution_profile = "claude-sdk".into(),
            "network_request" => {
                changed.profile.as_mut().unwrap().network.binding.request_id =
                    Uuid::new_v4().to_string()
            }
            "network_project" => changed
                .profile
                .as_mut()
                .unwrap()
                .network
                .binding
                .project_id
                .push('x'),
            _ => unreachable!(),
        }
        assert!(
            validate_network_frame(&changed, &"a".repeat(64), &frame(&changed)).is_err(),
            "{case}"
        );
    }
    assert!(validate_network_frame(&binding, &"b".repeat(64), &frame(&binding)).is_err());
    let mut changed = binding.clone();
    let ExecutionWorkspaceSelectionV4::RemoteSourceOnly { selection, .. } =
        &mut changed.profile.as_mut().unwrap().workspace
    else {
        unreachable!()
    };
    selection.root.root = ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"another root");
    assert!(
        receive_network_backend(event(&binding, 4), vec![], &changed, &"a".repeat(64))
            .err()
            .unwrap()
            .to_string()
            .contains("full workspace binding")
    );
}
#[test]
fn actual_socket_transfer_rejects_json_only_and_substituted_kernel_descriptors() {
    let binding = binding();
    for case in [
        "json",
        "regular",
        "host_namespaces",
        "reversed_namespaces",
        "non_pidfd",
    ] {
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        let process = pidfd(std::process::id()).unwrap();
        let user: OwnedFd = std::fs::File::open("/proc/self/ns/user").unwrap().into();
        let net: OwnedFd = std::fs::File::open("/proc/self/ns/net").unwrap().into();
        let regular: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let descriptors = match case {
            "json" => vec![],
            "regular" => vec![
                process.as_raw_fd(),
                regular.as_raw_fd(),
                regular.as_raw_fd(),
            ],
            "host_namespaces" => vec![process.as_raw_fd(), user.as_raw_fd(), net.as_raw_fd()],
            "reversed_namespaces" => vec![process.as_raw_fd(), net.as_raw_fd(), user.as_raw_fd()],
            "non_pidfd" => vec![regular.as_raw_fd(), user.as_raw_fd(), net.as_raw_fd()],
            _ => unreachable!(),
        };
        send_control_packet_fds(&mut sender, &event(&binding, 4), &descriptors).unwrap();
        let (received, fds): (ControlEvent, _) = receive_control_packet_fds(&mut receiver).unwrap();
        assert_eq!(fds.len(), descriptors.len());
        assert!(
            receive_network_backend(received, fds, &binding, &"a".repeat(64)).is_err(),
            "{case}"
        );
    }
}

// Explicit kernel qualification only. Running this ignored test requires a
// disposable UID/GID1500 Linux VM with unprivileged USER/NET namespaces enabled.
// No FUSE, source session, managed runtime or provider capability is exercised.
mod positive_kernel {
    use super::*;
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::{fs::PermissionsExt, process::CommandExt},
        path::Path,
        process::{Child, Command, Stdio},
    };

    const HELPER: &str = "execution::handoff::readiness_tests::positive_kernel::managed_child";
    const BACKEND: &str = "execution::handoff::readiness_tests::positive_kernel::backend_child";
    const ROUTE: &str = "RUDDER_READINESS_KERNEL_ROUTE";
    const PARENT: &str = "RUDDER_READINESS_KERNEL_PARENT";
    const FAMILY: &str = "RUDDER_READINESS_KERNEL_FAMILY";
    const SOCKET: &str = "RUDDER_READINESS_KERNEL_SOCKET";

    struct OwnedChild(Child);
    impl OwnedChild {
        fn finish(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    assert!(status.success(), "kernel fixture child failed: {status}");
                    return;
                }
                assert!(Instant::now() < deadline, "kernel fixture exit deadline");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            // Closing the helper's release pipe also unwinds/reaps its backend
            // on parent assertion failure. The backend additionally has an
            // inherited PDEATHSIG fence throughout unshare/setpriv/exec.
            drop(self.0.stdin.take());
            let deadline = Instant::now() + Duration::from_secs(2);
            while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn fixture(path: &Path) -> (ExecutionSpec, ManagedExecutionBinding) {
        let mut binding = binding();
        for name in ["authority", "broker"] {
            fs::create_dir(path.join(name)).unwrap();
            fs::set_permissions(path.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let context = path.join("authority/context.json");
        fs::write(&context, b"{}").unwrap();
        fs::set_permissions(&context, fs::Permissions::from_mode(0o600)).unwrap();
        let profile = binding.profile.as_mut().unwrap();
        let ExecutionWorkspaceSelectionV4::RemoteSourceOnly {
            selection,
            host_context,
        } = &mut profile.workspace
        else {
            unreachable!()
        };
        *host_context = ProtectedHostContextRef {
            format_version: 1,
            path: context,
            sha256: hex::encode(Sha256::digest(b"{}")),
            size_bytes: 2,
        };
        let observation = selection.observation.clone();
        let identity = crate::agent_api::AgentIdentityFile {
            format_version: 1,
            project_id: selection.attempt.project_id.clone(),
            run_id: binding.run_id.clone(),
            run_epoch: binding.run_epoch,
            owner_epoch: binding.owner.epoch,
            token: "synthetic-unusable-readiness-only".into(),
        };
        fs::write(
            path.join("broker/identity.json"),
            canonical_json(&identity).unwrap(),
        )
        .unwrap();
        fs::set_permissions(
            path.join("broker/identity.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let spec = ExecutionSpec {
            format_version: 5,
            execution_id: binding.execution_id.clone(),
            operation_id: binding.operation_id.clone(),
            execution_root: path.join("executions"),
            manager_root: path.join("manager"),
            store_root: path.join("store"),
            authority_roots: vec![path.join("authority")],
            view_id: binding.view_id.clone(),
            owner: binding.owner.clone(),
            command: vec!["/usr/bin/true".into()],
            environment: BTreeMap::new(),
            runtime: vec![],
            home_files: vec![],
            broker_directory: Some(path.join("broker")),
            broker_binding: Some(BrokerBinding {
                project_id: identity.project_id,
                run_id: binding.run_id.clone(),
                run_epoch: binding.run_epoch,
                owner_epoch: binding.owner.epoch,
            }),
            workspace_observation: Some(WorkspaceObservationProfile {
                binding: observation,
                endpoint_directory: path.join("rpc"),
                application: None,
            }),
            profile: binding.profile.clone(),
            timeout_seconds: 15,
        };
        spec.validate().unwrap();
        binding.validate(&spec).unwrap();
        (spec, binding)
    }

    #[test]
    #[ignore = "explicit disposable UID1500 Linux kernel qualification"]
    fn backend_child() {
        // All namespace/capability effects occurred in fresh external execs,
        // before libtest started threads. No unshare occurs in a test thread.
        let socket = std::env::var_os(SOCKET).expect("explicit fixture socket");
        let user: OwnedFd = fs::File::open("/proc/self/ns/user").unwrap().into();
        let net: OwnedFd = fs::File::open("/proc/self/ns/net").unwrap().into();
        let mut stream = UnixStream::connect(socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        send_descriptors(&stream, &[user.as_raw_fd(), net.as_raw_fd()]).unwrap();
        let mut release = [0];
        stream.read_exact(&mut release).unwrap();
        assert_eq!(release, [1]);
    }

    #[test]
    #[ignore = "internal child for explicit kernel readiness qualification"]
    fn managed_child() {
        let parent: u32 = std::env::var(PARENT).unwrap().parse().unwrap();
        let (spec, mut control) = receive_managed(&std::env::var(ROUTE).unwrap(), parent).unwrap();
        assert_eq!(spec.format_version, 5);
        assert_eq!(control.binding.format_version, 4);
        control.controller.check_current().unwrap();
        control.binding.validate(&spec).unwrap();
        let digest = hex::encode(Sha256::digest(canonical_json(&spec).unwrap()));
        let socket = spec
            .workspace_observation
            .as_ref()
            .unwrap()
            .endpoint_directory
            .parent()
            .unwrap()
            .join("backend.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut command = Command::new("/usr/bin/unshare");
        command
            .args([
                "--user",
                "--map-current-user",
                // Keep setup privileges only until trusted setpriv clears all
                // five sets; exec as a nonzero UID otherwise loses SETPCAP
                // before it can drop the bounding set.
                "--keep-caps",
                "--net",
                "--",
                "/usr/bin/setpriv",
                "--bounding-set=-all",
                "--inh-caps=-all",
                "--ambient-caps=-all",
                "--no-new-privs",
                "--",
            ])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                BACKEND,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env(SOCKET, &socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // Only async-signal-safe calls in the post-fork child. The same actual
        // PID must survive unshare, setpriv and backend exec (no --fork).
        let helper_pid = std::process::id() as i32;
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                    || libc::getppid() != helper_pid
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = OwnedChild(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut backend = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        child.0.try_wait().unwrap().is_none(),
                        "USER/NET namespace or setpriv unavailable; NOT qualified"
                    );
                    assert!(Instant::now() < deadline, "backend descriptor deadline");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("backend accept: {error}"),
            }
        };
        backend
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        backend
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(peer(&backend).unwrap().pid, child.0.id() as i32);
        let (tag, descriptors) = receive_descriptors(&backend).unwrap();
        assert_eq!(tag, 1);
        let process = pidfd(child.0.id()).unwrap();
        let mut ready_frame = frame(&control.binding);
        ready_frame.execution_request_digest = digest.clone();
        let ready = super::super::super::network_phase::OwnedNetworkReady::receive(
            &spec,
            &ready_frame.child_nonce,
            ready_frame.clone(),
            &process,
            child.0.id(),
            descriptors,
        )
        .unwrap();
        ready.validate_backend(&process).unwrap();
        let mut old = event(&control.binding, 3);
        if let ControlEvent::BackendReadyV3 { network, .. } = &mut old {
            *network = ready_frame;
        }
        let ns = ready.descriptors();
        // Reject old family with the SAME live, valid descriptors that the
        // positive readiness immediately uses, rather than missing/fake FDs.
        let old_fds = vec![
            process.try_clone().unwrap(),
            fs::File::open(format!("/proc/self/fd/{}", ns[0]))
                .unwrap()
                .into(),
            fs::File::open(format!("/proc/self/fd/{}", ns[1]))
                .unwrap()
                .into(),
        ];
        assert!(
            receive_network_backend(old, old_fds, &control.binding, &digest)
                .err()
                .unwrap()
                .to_string()
                .contains("protocol family")
        );
        if std::env::var(FAMILY).unwrap() == "4" {
            control.backend_ready_network(&process, &ready).unwrap();
        } else {
            let mut old = event(&control.binding, 3);
            if let ControlEvent::BackendReadyV3 { network, .. } = &mut old {
                *network = ready.frame().clone();
            }
            send_control_packet_fds(
                &mut control.stream,
                &old,
                &[process.as_raw_fd(), ns[0], ns[1]],
            )
            .unwrap();
        }
        let mut release = [0];
        std::io::stdin().read_exact(&mut release).unwrap();
        assert_eq!(release, [1]);
        backend.write_all(&release).unwrap();
        child.finish();
        assert!(ready.validate_backend(&process).is_err());
        fs::remove_file(socket).unwrap();
    }

    #[test]
    #[ignore = "requires disposable Linux UID/GID1500 with USER/NET grant; unsupported fails"]
    fn managed_binding4_accepts_real_private_backend_and_rejects_old_family() {
        assert_eq!(unsafe { libc::getuid() }, 1500, "requires explicit UID1500");
        assert_eq!(unsafe { libc::getgid() }, 1500, "requires explicit GID1500");
        for family in [4, 3] {
            // Short owned paths: both workspace metadata and AF_UNIX have
            // deliberate path bounds. No preexisting project state is read.
            let temp = tempfile::Builder::new()
                .prefix("rdy4-")
                .tempdir_in("/workspace")
                .unwrap();
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let (spec, binding) = fixture(temp.path());
            let refusal = crate::execution::linux::run_helper(spec.clone())
                .err()
                .unwrap();
            assert!(refusal
                .to_string()
                .contains("specification5 awaits owned source"));
            assert!(
                !spec.execution_root.exists(),
                "early refusal has execution effects"
            );
            let handoff = ManagedExecutionHandoff::new(&spec, binding.clone()).unwrap();
            let args = handoff.helper_args();
            let mut child = OwnedChild(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        HELPER,
                        "--ignored",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env_clear()
                    .env(ROUTE, &args[4])
                    .env(PARENT, &args[6])
                    .env(FAMILY, family.to_string())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap(),
            );
            let (receipt, handle) = handoff.activate(child.0.id()).unwrap();
            receipt.wait(Duration::from_secs(3)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(12);
            let accepted = loop {
                if let Some(event) = handle.try_event() {
                    match event {
                        ManagedExecutionEvent::BackendReady { process } if family == 4 => {
                            process.check_current().unwrap();
                            assert!(process.matches_attempt(
                                &binding.run_id,
                                &binding.execution_id,
                                &binding.execution_profile
                            ));
                            assert!(!process.authenticated_start_required());
                            assert_ne!(process.pid(), child.0.id());
                            break Some(process);
                        }
                        ManagedExecutionEvent::Uncertain if family == 3 => break None,
                        _ => panic!("unexpected managed kernel readiness event"),
                    }
                }
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "kernel fixture failed; NOT qualified"
                );
                assert!(Instant::now() < deadline, "managed readiness deadline");
                std::thread::sleep(Duration::from_millis(5));
            };
            child.0.stdin.take().unwrap().write_all(&[1]).unwrap();
            child.finish();
            if let Some(process) = accepted {
                assert!(
                    process.check_current().is_err(),
                    "exited backend remained current"
                );
            }
            drop(handle);
        }
    }
}
