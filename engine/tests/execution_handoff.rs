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
    let source = path.join("handoff.rs");
    fs::write(&source,r#"
use rudder_engine::execution::handoff;
fn main(){let a=std::env::args().collect::<Vec<_>>();std::thread::sleep(std::time::Duration::from_millis(150));let spec=handoff::receive(&a[1],a[2].parse().unwrap()).unwrap();assert_eq!(spec.environment["CANARY_SECRET"],"known-fixture-secret-never-argv");assert!(!std::fs::read("/proc/self/cmdline").unwrap().windows(b"known-fixture-secret-never-argv".len()).any(|s|s==b"known-fixture-secret-never-argv"));assert!(!std::fs::read("/proc/self/environ").unwrap().windows(b"known-fixture-secret-never-argv".len()).any(|s|s==b"known-fixture-secret-never-argv"));println!("handoff-ok");}
"#).unwrap();
    let deps = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let library = fs::read_dir(&deps)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("librudder_engine-")
                && e.path().extension().is_some_and(|e| e == "rlib")
        })
        .max_by_key(|e| e.metadata().unwrap().modified().unwrap())
        .unwrap()
        .path();
    let binary = path.join("handoff");
    let r = Command::new("rustc")
        .arg("--edition=2021")
        .arg(source)
        .args([
            "--extern",
            &format!("rudder_engine={}", library.display()),
            "-L",
            &format!("dependency={}", deps.display()),
            "-o",
        ])
        .arg(&binary)
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
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
