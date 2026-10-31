#![cfg(target_os = "linux")]
use rudder_engine::{
    execution::{
        handoff::ExecutionHandoff,
        linux::{read_spec_fd, sealed_spec_fd},
        ExecutionSpec, HomeFile,
    },
    protocol::OwnerEpoch,
};
use std::{
    collections::BTreeMap, fs, os::fd::AsRawFd, path::Path, process::Command, time::Duration,
};
use uuid::Uuid;
fn spec(path: &Path) -> ExecutionSpec {
    ExecutionSpec {
        profile: None,
        format_version: 1,
        execution_id: Uuid::new_v4().to_string(),
        operation_id: Uuid::new_v4().to_string(),
        execution_root: path.join("executions"),
        manager_root: path.join("manager"),
        store_root: path.join("cas"),
        authority_roots: vec![path.join("journal")],
        view_id: Uuid::new_v4().to_string(),
        owner: OwnerEpoch {
            owner: "execution-test".into(),
            epoch: 1,
        },
        command: vec!["/bin/sh".into(), "-c".into(), "true".into()],
        environment: BTreeMap::from([(
            "CANARY_SECRET".into(),
            "known-fixture-secret-never-argv".into(),
        )]),
        runtime: vec![],
        home_files: vec![HomeFile {
            relative_path: ".config/own-auth.json".into(),
            contents: b"known fixture bytes".to_vec(),
        }],
        broker_directory: None,
        broker_binding: None,
        workspace_observation: None,
        timeout_seconds: 30,
    }
}
#[test]
fn sealed_payload_rejects_mutation_and_regular_file_substitution() {
    let t = tempfile::tempdir().unwrap();
    let s = spec(t.path());
    let fd = sealed_spec_fd(&s).unwrap();
    assert_eq!(
        unsafe { libc::pwrite(fd.as_raw_fd(), b"x".as_ptr().cast(), 1, 0) },
        -1
    );
    let parsed = read_spec_fd(fd).unwrap();
    assert_eq!(parsed.environment, s.environment);
    let path = t.path().join("not-sealed");
    fs::write(&path, b"{}").unwrap();
    assert!(read_spec_fd(fs::File::open(path).unwrap().into()).is_err());
}
#[test]
fn private_authority_and_home_projection_policy_fails_closed() {
    use rudder_engine::execution::ReadOnlyRuntime;
    let t = tempfile::tempdir().unwrap();
    let mut s = spec(t.path());
    s.runtime.push(ReadOnlyRuntime {
        source: "/".into(),
        guest_path: "/usr".into(),
    });
    assert!(s.validate().is_err());
    s.runtime = vec![ReadOnlyRuntime {
        source: "/usr".into(),
        guest_path: "/".into(),
    }];
    assert!(s.validate().is_err());
    s.runtime = vec![ReadOnlyRuntime {
        source: "/usr".into(),
        guest_path: "/usr".into(),
    }];
    s.authority_roots = vec!["/usr/private-journal-not-yet-created".into()];
    assert!(s.validate().is_err());
    s.runtime.clear();
    s.home_files[0].relative_path = "../peer/auth.json".into();
    assert!(s.validate().is_err());
}
#[test]
fn broker_projection_allows_lazy_socket_but_rejects_extra_or_symlink_identity() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let t = tempfile::tempdir().unwrap();
    let broker = t.path().join("broker");
    fs::create_dir(&broker).unwrap();
    fs::set_permissions(&broker, fs::Permissions::from_mode(0o700)).unwrap();
    let mut s = spec(t.path());
    s.broker_directory = Some(broker.clone());
    s.broker_binding = Some(rudder_engine::execution::BrokerBinding {
        project_id: "p".into(),
        run_id: "a".into(),
        run_epoch: 1,
        owner_epoch: 1,
    });
    assert!(s.validate().is_err());
    fs::write(broker.join("identity.json"),br#"{"format_version":1,"project_id":"p","run_id":"a","run_epoch":1,"owner_epoch":1,"token":"fixture-token-not-authenticated"}"#).unwrap();
    fs::set_permissions(
        broker.join("identity.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    s.validate().unwrap();
    fs::write(
        broker.join("journal.sqlite"),
        b"private authority must not be projected",
    )
    .unwrap();
    assert!(s.validate().is_err());
    fs::remove_file(broker.join("journal.sqlite")).unwrap();
    fs::remove_file(broker.join("identity.json")).unwrap();
    fs::write(t.path().join("peer"), b"{}").unwrap();
    symlink(t.path().join("peer"), broker.join("identity.json")).unwrap();
    assert!(s.validate().is_err());
}

fn helper(path: &Path) -> std::path::PathBuf {
    let binary = path.join("handoff");
    fs::copy(env!("CARGO_BIN_EXE_rudder-execution-handoff-test"), &binary).unwrap();
    binary
}
#[test]
fn only_actual_child_receives_one_use_spec_after_other_peer_rejection() {
    use std::io::Read;
    use std::os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixStream},
    };
    let t = tempfile::tempdir().unwrap();
    let binary = helper(t.path());
    let h = ExecutionHandoff::new(&spec(t.path())).unwrap();
    let args = h.helper_args();
    let child = Command::new(binary)
        .args([&args[4], &args[6]])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let receipt = h.activate(child.id()).unwrap();
    let address = SocketAddr::from_abstract_name(args[4].as_bytes()).unwrap();
    let mut impostor = UnixStream::connect_addr(&address).unwrap();
    impostor
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut byte = [0u8];
    assert_eq!(impostor.read(&mut byte).unwrap(), 0);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "handoff-ok"
    );
    receipt.wait(Duration::from_secs(2)).unwrap();
    assert!(UnixStream::connect_addr(&address).is_err());
}

// Only this test helper owns the spawned fixture; failure reaps it before the
// temporary routing/spec files are removed. No production process is signaled.
struct ControllerFixtureChild(Option<std::process::Child>);
impl ControllerFixtureChild {
    fn output(mut self) -> std::process::Output {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "managed controller fixture did not exit before deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}
impl Drop for ControllerFixtureChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn managed_handoff_retains_actual_controller_witness_after_control_close() {
    use rudder_engine::{
        execution::{
            handoff::ManagedExecutionHandoff, BrokerBinding, HelperApplicationSpec,
            ManagedExecutionBinding, ManagedFrontendStart, ProtectedHostContextRef,
            WorkspaceObservationProfile,
        },
        protocol::{ObjectKind, ObjectRef},
        vfs::rpc::{AttemptContext, ObservationBinding},
    };
    use std::os::{
        linux::net::SocketAddrExt,
        unix::{
            fs::PermissionsExt,
            net::{SocketAddr, UnixListener},
        },
    };
    let temp = tempfile::Builder::new()
        .prefix("controller-witness-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut value = spec(temp.path());
    value.format_version = 3;
    let broker = temp.path().join("broker");
    fs::create_dir(&broker).unwrap();
    fs::set_permissions(&broker, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(broker.join("identity.json"), br#"{"format_version":1,"project_id":"p","run_id":"r","run_epoch":1,"owner_epoch":1,"token":"fixture-token-never-authorized"}"#).unwrap();
    fs::set_permissions(
        broker.join("identity.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    value.broker_directory = Some(broker);
    value.broker_binding = Some(BrokerBinding {
        project_id: "p".into(),
        run_id: "r".into(),
        run_epoch: 1,
        owner_epoch: value.owner.epoch,
    });
    let authority = temp.path().join("journal");
    fs::create_dir(&authority).unwrap();
    fs::set_permissions(&authority, fs::Permissions::from_mode(0o700)).unwrap();
    let context = authority.join("context.json");
    fs::write(&context, b"{}").unwrap();
    fs::set_permissions(&context, fs::Permissions::from_mode(0o600)).unwrap();
    value.workspace_observation = Some(WorkspaceObservationProfile {
        binding: ObservationBinding {
            format_version: 1,
            attempt: AttemptContext {
                execution_id: value.execution_id.clone(),
                operation_id: value.operation_id.clone(),
            },
            view_id: value.view_id.clone(),
            owner: value.owner.clone(),
            namespace: Uuid::new_v4().to_string(),
            profile: ObjectRef::from_bytes(ObjectKind::Blob, b"handoff-only fixture profile"),
            exclusion_policy_digest: "a".repeat(64),
        },
        endpoint_directory: temp.path().join("workspace"),
        application: Some(HelperApplicationSpec {
            format_version: 1,
            kind: "handoff-fixture-v1".into(),
            host_context: ProtectedHostContextRef {
                format_version: 1,
                path: context,
                sha256: ObjectRef::from_bytes(ObjectKind::Blob, b"{}").digest,
                size_bytes: 2,
            },
            projection_directory: temp.path().join("application"),
        }),
    });
    let binding = ManagedExecutionBinding {
        format_version: 2,
        request_id: Uuid::new_v4().to_string(),
        run_id: "r".into(),
        run_epoch: 1,
        execution_id: value.execution_id.clone(),
        operation_id: value.operation_id.clone(),
        view_id: value.view_id.clone(),
        owner: value.owner.clone(),
        execution_profile: "claude-sdk".into(),
        frontend_start: ManagedFrontendStart::Immediate,
        server_command: None,
        guest_socket: "/tmp/cx.sock".into(),
        profile: None,
    };
    value.validate().unwrap();
    let handoff = ManagedExecutionHandoff::new(&value, binding).unwrap();
    let args = handoff.helper_args();
    let peer_name = format!("rudder-controller-witness-{}", Uuid::new_v4());
    let listener = UnixListener::bind_addr(
        &SocketAddr::from_abstract_name(peer_name.as_bytes()).unwrap(),
    )
    .unwrap();
    listener.set_nonblocking(true).unwrap();
    let child = ControllerFixtureChild(Some(
        Command::new(env!("CARGO_BIN_EXE_rudder-execution-handoff-test"))
            .args([
                args[4].as_str(),
                args[6].as_str(),
                "managed-controller",
                peer_name.as_str(),
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let (receipt, handle) = handoff.activate(child.0.as_ref().unwrap().id()).unwrap();
    receipt.wait(Duration::from_secs(2)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let _stream = loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                break stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "fixture did not connect to the actual controller"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("controller witness fixture accept failed: {error}"),
        }
    };
    let output = child.output();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "managed-controller-witness-ok"
    );
    drop(handle);
}
