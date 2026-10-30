use super::*;
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        // Darwin's TMPDIR can already approach sun_path's limit. These tests
        // bind real sockets, so use a short, private directory explicitly.
        let path = PathBuf::from("/tmp").join(format!("eng-{}", uuid::Uuid::new_v4().simple()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn record(repo: &Path) -> OwnerRecord {
    OwnerRecord {
        format_version: ENGINE_FORMAT,
        repo: repo.into(),
        authority: "fabric".into(),
        implementation: "legacy-native-extraction".into(),
        pid: std::process::id(),
        owner: RecordedFence {
            project_id: "local:test".into(),
            owner_id: "fabric:test".into(),
            epoch: 7,
        },
        instance_id: "instance-current".into(),
        attach_socket: repo.join(".rudder/attach.sock"),
        control_socket: Some(repo.join("control.sock")),
        state: "ready".into(),
        started_at: "test".into(),
        control_nonce: Some("private-control-token".into()),
    }
}

#[test]
fn public_status_never_exposes_the_control_nonce() {
    let scratch = Scratch::new();
    let value = record(&scratch.0).public(true);
    assert_eq!(value["owner"]["epoch"], 7);
    assert_eq!(value["implementation"], "legacy-native-extraction");
    assert!(!value.to_string().contains("private-control-token"));
    assert!(value.get("control_nonce").is_none());
}

#[test]
fn control_requires_the_current_instance_epoch_and_nonce() {
    let scratch = Scratch::new();
    let owner = record(&scratch.0);
    let mut request = ControlRequest {
        format_version: 1,
        op: "shutdown".into(),
        instance_id: owner.instance_id.clone(),
        epoch: 7,
        nonce: "private-control-token".into(),
    };
    assert!(request.authenticates(&owner));
    request.epoch = 6;
    assert!(
        !request.authenticates(&owner),
        "stale owner epoch cannot stop a new service"
    );
    request.epoch = 7;
    request.instance_id = "instance-old".into();
    assert!(
        !request.authenticates(&owner),
        "PID reuse does not confer control"
    );
    request.instance_id = owner.instance_id.clone();
    request.nonce = "wrong".into();
    assert!(!request.authenticates(&owner));
}

#[test]
fn checkout_lock_excludes_a_second_owner_and_keeps_one_inode() {
    let scratch = Scratch::new();
    let first = acquire_checkout_lock(&scratch.0).unwrap();
    let inode = first.metadata().unwrap().ino();
    assert!(acquire_checkout_lock(&scratch.0).is_err());
    assert!(ownership_lock_held(&scratch.0).unwrap());
    drop(first);
    assert!(!ownership_lock_held(&scratch.0).unwrap());
    let second = acquire_checkout_lock(&scratch.0).unwrap();
    assert_eq!(
        second.metadata().unwrap().ino(),
        inode,
        "never unlink a lock with possible waiters"
    );
}

#[test]
fn checkout_release_unlocks_even_while_an_inherited_descriptor_remains_open() {
    let scratch = Scratch::new();
    let first = acquire_checkout_lock(&scratch.0).unwrap();
    // dup shares the same open file description, as an in-flight fork does
    // before close-on-exec. No child scheduling race is needed to reproduce it.
    let inherited = first.try_clone().unwrap();
    drop(first);
    let second = acquire_checkout_lock(&scratch.0).unwrap();
    assert!(ownership_lock_held(&scratch.0).unwrap());
    drop(inherited);
    assert!(ownership_lock_held(&scratch.0).unwrap(), "an old descriptor cannot release the replacement owner");
    drop(second);
    assert!(!ownership_lock_held(&scratch.0).unwrap());
}

#[test]
fn old_legacy_attach_owner_is_refused_before_journal_or_app_start() {
    let scratch = Scratch::new();
    fs::create_dir(scratch.0.join(".rudder")).unwrap();
    let socket = scratch.0.join(".rudder/attach.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let error = EngineOwnership::acquire(&scratch.0, NativeAuthority::Fabric)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("live legacy dashboard"),
        "{error:#}"
    );
    assert!(!scratch.0.join(".rudder/engine/owner.json").exists());
    assert!(!ownership_lock_held(&scratch.0).unwrap());
}

fn long_checkout(scratch: &Scratch) -> PathBuf {
    let path = scratch.0.join("long-checkout-path-".repeat(12));
    fs::create_dir(&path).unwrap();
    assert!(path.join(".rudder/attach.sock").as_os_str().len() > 108);
    path
}

#[test]
fn long_checkout_without_an_attach_endpoint_is_quiescent_without_side_effects() {
    let scratch = Scratch::new();
    let repo = long_checkout(&scratch);
    let path = repo.join(".rudder/attach.sock");
    assert!(!attach_is_live(&path).unwrap());
    assert_eq!(engine_status(&repo, false).unwrap()["running"], false);
    assert!(!repo.join(".rudder").exists(), "status is read-only");
    fs::create_dir(repo.join(".rudder")).unwrap();
    assert!(!attach_is_live(&path).unwrap());
}

#[test]
fn long_checkout_resolves_live_short_attach_symlinks_and_refuses_migration() {
    let scratch = Scratch::new();
    let repo = long_checkout(&scratch);
    fs::create_dir(repo.join(".rudder")).unwrap();
    let target = scratch.0.join("live.sock");
    let _listener = UnixListener::bind(&target).unwrap();
    symlink(&target, repo.join(".rudder/attach.sock")).unwrap();
    assert!(attach_is_live(&repo.join(".rudder/attach.sock")).unwrap());
    let error = EngineOwnership::acquire(&repo, NativeAuthority::Fabric).err().unwrap();
    assert!(error.to_string().contains("live legacy dashboard"), "{error:#}");
    assert!(!repo.join(".rudder/engine/owner.json").exists());
    assert!(!ownership_lock_held(&repo).unwrap());
}

#[test]
fn long_checkout_relative_and_dangling_attach_links_are_classified_before_connect() {
    let scratch = Scratch::new();
    let repo = long_checkout(&scratch);
    fs::create_dir(repo.join(".rudder")).unwrap();
    let path = repo.join(".rudder/attach.sock");
    let target = scratch.0.join("relative.sock");
    let listener = UnixListener::bind(&target).unwrap();
    symlink("../../relative.sock", &path).unwrap();
    assert!(attach_is_live(&path).unwrap(), "long relative spelling resolves to a short live socket");
    drop(listener);
    assert!(!attach_is_live(&path).unwrap(), "a refused stale socket is quiescent");
    fs::remove_file(&path).unwrap();
    symlink(scratch.0.join("absent.sock"), &path).unwrap();
    assert!(!attach_is_live(&path).unwrap());
    fs::remove_file(&path).unwrap();
    symlink("../../absent.sock", &path).unwrap();
    assert!(!attach_is_live(&path).unwrap());
    fs::remove_file(&path).unwrap();
    symlink("loop.sock", &path).unwrap();
    symlink("attach.sock", repo.join(".rudder/loop.sock")).unwrap();
    assert!(attach_is_live(&path).is_err(), "a symlink loop cannot establish quiescence");
}

#[test]
fn unexpected_attach_types_and_permission_errors_never_authorize_migration() {
    let scratch = Scratch::new();
    fs::create_dir(scratch.0.join(".rudder")).unwrap();
    let path = scratch.0.join(".rudder/attach.sock");
    fs::write(&path, "unrelated file").unwrap();
    assert!(attach_is_live(&path).is_err());
    let error = EngineOwnership::acquire(&scratch.0, NativeAuthority::Fabric).err().unwrap();
    assert!(error.to_string().contains("quiescent"), "{error:#}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "unrelated file");
    assert!(!scratch.0.join(".rudder/engine/owner.json").exists());
    fs::remove_file(&path).unwrap();
    let blocked = scratch.0.join("blocked");
    fs::create_dir(&blocked).unwrap();
    let _listener = UnixListener::bind(blocked.join("live.sock")).unwrap();
    symlink(blocked.join("live.sock"), &path).unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    let result = attach_is_live(&path);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        let error = result.err().expect("permission denied is not proof that a socket is absent");
        assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
    } else {
        assert!(result.unwrap(), "privileged test process can still observe the live socket");
    }
}

#[test]
fn absent_status_is_read_only_and_stale_pid_is_never_signaled() {
    let scratch = Scratch::new();
    assert_eq!(engine_status(&scratch.0, false).unwrap()["running"], false);
    assert!(
        !scratch.0.join(".rudder").exists(),
        "a read-only status creates no owner state"
    );
    fs::create_dir_all(scratch.0.join(".rudder/engine")).unwrap();
    write_owner_record(
        &scratch.0.join(".rudder/engine/owner.json"),
        &record(&scratch.0),
    )
    .unwrap();
    // The record contains this test process's PID. Shutdown must consult the
    // socket+lock, not signal that stale/reused informational PID.
    assert_eq!(engine_status(&scratch.0, true).unwrap()["running"], false);
    fs::write(
        scratch.0.join(".rudder/engine/owner.json"),
        "corrupt stale projection",
    )
    .unwrap();
    assert_eq!(
        engine_status(&scratch.0, false).unwrap()["running"],
        false,
        "an inactive derived projection cannot block a new explicit owner"
    );
}

#[test]
fn control_frames_reject_oversized_incomplete_and_unknown_fields() {
    for payload in [
        vec![b'x'; CONTROL_LIMIT + 1],
        b"{}".to_vec(),
        b"{\"unexpected\":true}\n".to_vec(),
    ] {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        writer.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // Exercise backpressure on Linux too. Darwin's stream default is
            // 8192 bytes, one byte less than this oversized-frame fixture.
            let size: libc::c_int = 1024;
            assert_eq!(unsafe {
                libc::setsockopt(writer.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF,
                    &size as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&size) as libc::socklen_t)
            }, 0);
        }
        let oversized = payload.len() > CONTROL_LIMIT;
        let sender = thread::spawn(move || -> io::Result<()> {
            writer.write_all(&payload)?;
            writer.shutdown(std::net::Shutdown::Write)
        });
        let error = read_control(&reader).err().expect("invalid control frame was accepted");
        sender.join().unwrap().unwrap();
        if oversized {
            assert!(error.to_string().contains("invalid engine control request size"),
                "the parser must reject all 8193 bytes, not pass after an I/O timeout: {error:#}");
        }
    }
}

#[test]
fn accepted_nonblocking_control_stream_waits_for_fragments_and_retains_its_deadline() {
    use std::os::fd::AsRawFd;
    let (reader, mut writer) = UnixStream::pair().unwrap();
    // Force Darwin's accepted-socket state on every Unix test host.
    reader.set_nonblocking(true).unwrap();
    prepare_control_stream(&reader).unwrap();
    let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(flags & libc::O_NONBLOCK, 0);
    let payload = serde_json::to_vec(&ControlRequest {
        format_version: 1,
        op: "status".into(),
        instance_id: "fragmented-instance".into(),
        epoch: 7,
        nonce: "public-control-fixture".into(),
    })
    .unwrap();
    let split = payload.len() / 2;
    writer.write_all(&payload[..split]).unwrap();
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        writer.write_all(&payload[split..]).unwrap();
        writer.write_all(b"\n").unwrap();
    });
    let request = read_control(&reader).expect("wait for the rest of a valid control frame");
    sender.join().unwrap();
    assert_eq!(request.op, "status");
    assert_eq!(request.instance_id, "fragmented-instance");

    let (reader, mut writer) = UnixStream::pair().unwrap();
    reader.set_nonblocking(true).unwrap();
    prepare_control_stream(&reader).unwrap();
    writer.write_all(b"{").unwrap();
    let started = Instant::now();
    assert!(read_control(&reader).is_err(), "an incomplete live peer cannot wait forever");
    assert!(started.elapsed() >= Duration::from_millis(100), "inherited EAGAIN bypassed the deadline");
    assert!(started.elapsed() < Duration::from_secs(3), "the configured control deadline was lost");
    drop(writer);
}

#[test]
fn slow_control_fragments_cannot_extend_the_whole_request_deadline() {
    let (reader, mut writer) = UnixStream::pair().unwrap();
    prepare_control_stream(&reader).unwrap();
    let sender = thread::spawn(move || {
        for _ in 0..64 {
            if writer.write_all(b" ").is_err() { break; }
            thread::sleep(Duration::from_millis(60));
        }
    });
    let started = Instant::now();
    assert!(read_control(&reader).is_err());
    assert!(started.elapsed() < Duration::from_secs(2),
        "regular partial reads restarted the whole request deadline");
    drop(reader);
    sender.join().unwrap();
}

#[test]
fn machine_notices_preserve_confirmation_and_draft_then_project_once() {
    let scratch = Scratch::new();
    let mut app = App::new();
    app.cwd = scratch.0.clone();
    app.engine_headless = true;
    app.task_input = "my unsent local draft".into();
    app.notice = Some("press d again to confirm".into());
    app.delete_pending = Some("run-being-confirmed".into());
    app.publish_engine_notice("background operation failed".into());
    app.project_engine_notice();
    assert_eq!(app.notice.as_deref(), Some("press d again to confirm"));
    assert_eq!(app.machine_notice_cursor, 0);
    assert_eq!(app.task_input, "my unsent local draft");
    app.delete_pending = None;
    app.project_engine_notice();
    assert_eq!(app.notice.as_deref(), Some("background operation failed"));
    assert_eq!(app.machine_notice_cursor, 1);
    app.notice = Some("a newer view-local result".into());
    app.project_engine_notice();
    assert_eq!(
        app.notice.as_deref(),
        Some("a newer view-local result"),
        "one machine event does not overwrite every frame"
    );
    let activity = fs::read_to_string(scratch.0.join(".rudder/activity.jsonl")).unwrap();
    assert!(activity.contains("engine notice: background operation failed"));
}

#[test]
fn fabric_refuses_live_legacy_processes_without_changing_their_records() {
    let scratch = Scratch::new();
    let run = scratch.0.join(".rudder/runs/old-worker");
    fs::create_dir_all(&run).unwrap();
    let data = serde_json::json!({"id":"old-worker","status":"running","process":{"controllerPid":std::process::id()}}).to_string();
    fs::write(run.join("run.json"), &data).unwrap();
    let error = EngineOwnership::acquire(&scratch.0, NativeAuthority::Fabric)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("live legacy writer"),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(run.join("run.json")).unwrap(), data);
    assert!(!scratch.0.join(".rudder/engine/owner.json").exists());
    assert!(!ownership_lock_held(&scratch.0).unwrap());
}

#[test]
fn fabric_refuses_scheduler_but_permits_read_only_projector_metadata() {
    let scratch = Scratch::new();
    fs::create_dir(scratch.0.join(".rudder")).unwrap();
    let file = scratch.0.join(".rudder/daemon.pid");
    fs::write(
        &file,
        serde_json::json!({"pid":std::process::id(),"controlMode":"scheduler"}).to_string(),
    )
    .unwrap();
    assert!(reject_legacy_writers(&scratch.0).is_err());
    fs::write(
        &file,
        serde_json::json!({"pid":std::process::id()}).to_string(),
    )
    .unwrap();
    assert!(
        reject_legacy_writers(&scratch.0).is_err(),
        "an old unspecified control mode is uncertain"
    );
    fs::write(
        &file,
        serde_json::json!({"pid":std::process::id(),"controlMode":"projector"}).to_string(),
    )
    .unwrap();
    assert!(reject_legacy_writers(&scratch.0).is_ok());
}
