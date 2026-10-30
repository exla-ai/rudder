use super::*;
use std::os::unix::fs::MetadataExt;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = env::temp_dir().join(format!("eng-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir(&path).unwrap();
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
        writer.write_all(&payload).unwrap();
        writer.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(read_control(&reader).is_err());
    }
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
