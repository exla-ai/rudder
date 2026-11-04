//! Real App, real local sockets and owned fake backend PTYs. These tests do
//! not qualify model protocols, managed helpers, Cloud, or hardware surfaces.
use super::*;
use anyhow::anyhow;
use std::os::unix::{
    fs::DirBuilderExt,
    net::{UnixListener, UnixStream},
};

struct Harness {
    root: PathBuf,
    home: PathBuf,
    app: App,
    lifecycle: Option<AppEngineLifecycle>,
    owner: EngineOwnership,
}

impl Harness {
    fn new() -> Self {
        let storage = if cfg!(target_os = "linux") {
            PathBuf::from("/workspace/rudder-tmp")
        } else {
            std::env::temp_dir()
        };
        let root = storage.join(format!("app-engine-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&root).unwrap();
        let repository = root.join("repo");
        let home = root.join("home");
        fs::create_dir(&repository).unwrap();
        let repository = repository.canonicalize().unwrap();
        fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
        let mut owner =
            EngineOwnership::acquire_in_home(&repository, NativeAuthority::Fabric, &home).unwrap();
        let lifecycle = Some(AppEngineLifecycle::prepare(&mut owner).unwrap());
        let mut app = App::new();
        app.cwd = repository;
        app.engine_headless = true;
        Self {
            root,
            home,
            app,
            lifecycle,
            owner,
        }
    }

    fn start(&mut self) {
        self.lifecycle
            .as_mut()
            .unwrap()
            .start(&mut self.app, &mut self.owner)
            .unwrap();
    }

    fn fake_backend(&mut self) -> u32 {
        let pane = TerminalPane::spawn_shell_or_command(
            Some(TerminalCommand::with_args(
                "/bin/sh",
                ["-c", "printf 'fake backend ready\\n'; exec cat"],
            )),
            TerminalPaneOptions {
                size: TerminalSize { rows: 24, cols: 80 },
                ..Default::default()
            },
        )
        .unwrap();
        let mut run = crate::app_tests::test_agent_run_with_terminal(&self.app, pane);
        // A local fake backend has no qualified model transport or tool broker.
        run.plain_process = true;
        let pid = run.terminal.as_ref().unwrap().child_process_id().unwrap();
        self.app.agents.push(run);
        save_native_run_record(&self.app.cwd, &self.app.agents[0]).unwrap();
        pid
    }

    fn pump_until(&mut self, predicate: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(!self
                .lifecycle
                .as_mut()
                .unwrap()
                .poll(&mut self.app, &mut self.owner)
                .unwrap());
            self.app.service_attach_clients();
            if predicate(&self.app) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "App did not apply the actual socket event"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn surface(&self) -> UnixStream {
        let mut stream = UnixStream::connect(self.app.attach_socket_path()).unwrap();
        writeln!(
            stream,
            "{}",
            serde_json::to_string(&ClientMsg::Dashboard { rows: 24, cols: 80 }).unwrap()
        )
        .unwrap();
        stream
    }

    fn record(&self) -> serde_json::Value {
        serde_json::from_slice(
            &fs::read(native_run_dir(&self.app.cwd, "run-1").join("run.json")).unwrap(),
        )
        .unwrap()
    }

    fn screen(&mut self, run_id: &str) -> serde_json::Value {
        let owner = self.projection();
        let request = serde_json::json!({"format_version":1,"op":"screen","run_id":run_id,
            "instance_id":owner["instance_id"],"epoch":owner["owner"]["epoch"],
            "nonce":owner["control_nonce"]});
        let socket = PathBuf::from(owner["control_socket"].as_str().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let mut stream = UnixStream::connect(socket).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            writeln!(stream, "{request}").unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            tx.send(serde_json::from_str::<serde_json::Value>(&line).unwrap()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let result = loop {
            assert!(!self.lifecycle.as_mut().unwrap().poll(&mut self.app, &mut self.owner).unwrap());
            if let Ok(value) = rx.try_recv() { break value; }
            assert!(Instant::now() < deadline, "screen request did not settle");
            thread::sleep(Duration::from_millis(5));
        };
        thread.join().unwrap();
        result
    }

    fn projection(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.app.cwd.join(".rudder/engine/owner.json")).unwrap())
            .unwrap()
    }

    fn finish(&mut self, outcome: Result<()>) -> Result<()> {
        self.lifecycle
            .take()
            .unwrap()
            .finish(&mut self.app, &mut self.owner, outcome)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.app.close_attach_clients("test finished");
        self.app.agents.clear();
        self.lifecycle.take();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn managed_drain_keeps_actual_app_and_owner_until_source_worker_join_settles() {
    use crate::managed_source_app::{SourceJournalFactory, SourceOwnerLifetimeSupervisor};
    let (mut fixture, request) = crate::managed_launch::source_prepare::tests::request();
    let owner = fixture.owner.as_mut().unwrap();
    let fence = request.fence().clone();
    let mut app = App::new_managed(ManagedAppInitialState {
        repository: fixture.repo.clone(),
        selection: InitialSelection { backend: Backend::Codex, model: "offline-test".into(), effort: None },
        branch: "test".into(), session_started_iso: "2026-11-04T00:00:00Z".into(),
    });
    let marker = managed_launch::ManagedRunSource::Reserved {
        fence: fence.clone(), runnable_root: request.selection().root.root.clone(),
    };
    let mut run = crate::gitio::agent_from_run_record(&app.cwd, serde_json::json!({
        "id":fence.run_id,"task":"managed drain join", "mode":"execute", "managedLocal":marker,
    })).unwrap();
    run.managed_local = Some(marker);
    run.managed_store = Some(request.managed_run().unwrap());
    run.execution_profile = Some("offline-command-v1".into());
    run.status = AgentStatus::Running;
    app.agents.push(run);
    let supervisor = SourceOwnerLifetimeSupervisor::new_off_app().unwrap();
    app.configure_source_owner_supervisor(supervisor.clone()).unwrap();
    let writer = owner.source_journal_factory(&fence.owner).unwrap();
    let (entered, wait) = mpsc::sync_channel(1);
    let (release, held) = mpsc::sync_channel(1);
    let writer: SourceJournalFactory = Box::new(move || {
        entered.send(()).unwrap();
        held.recv_timeout(Duration::from_secs(4)).unwrap();
        writer()
    });
    app.queue_managed_source(0, request, PathBuf::from("/usr/bin/true"), writer, &fence.owner).unwrap();
    wait.recv_timeout(Duration::from_secs(4)).unwrap();
    let gate = managed_machine_entry::ManagedMachineStartupGate::closed_for_tests();
    let mut drain = ManagedAppDrain::capture(owner);
    assert!(!drain.poll(&mut app, owner, &gate).unwrap());
    assert!(app.ensure_managed_source_shutdown_ready().is_err());
    owner.assert_current().unwrap();
    assert!(app.shutdown_with_reason("drain still pending").is_err());
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    while !drain.poll(&mut app, owner, &gate).unwrap() {
        assert!(Instant::now() < deadline, "actual managed App drain did not settle");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(app.agents[0].terminal.is_none());
    assert_eq!(app.agents[0].status, AgentStatus::Paused);
    let (journal, fence) = owner.journal_parts();
    assert!(journal.project_state(&fence.project_id).unwrap().owner_active,
        "App joins cannot release machine-wide source/seed/Root authority");
    owner.assert_current().unwrap();
    assert!(drain.poll(&mut app, owner, &gate).unwrap());
    app.shutdown_with_reason("joined App, machine owner retained").unwrap();
    owner.assert_current().unwrap();
    assert!(supervisor.try_close_off_app().is_ok());
    assert!(!app.cwd.join("RUDDER.md").exists(), "managed shutdown must not write legacy context");
}

#[cfg(target_os = "linux")]
#[test]
fn managed_drain_refuses_wrong_checkout_and_changed_epoch_without_owner_release() {
    let mut h = Harness::new();
    h.app.managed_project_mode = true;
    let gate = managed_machine_entry::ManagedMachineStartupGate::closed_for_tests();
    let mut drain = ManagedAppDrain::capture(&h.owner);
    let repository = h.app.cwd.clone();
    h.app.cwd = h.home.clone();
    assert!(drain.poll(&mut h.app, &mut h.owner, &gate).is_err());
    h.owner.assert_current().unwrap();
    h.app.cwd = repository;
    let (journal, fence) = h.owner.journal_parts();
    let successor = journal.transfer_owner(&fence.clone(), "successor", 100).unwrap();
    assert!(drain.poll(&mut h.app, &mut h.owner, &gate).is_err());
    let (journal, _) = h.owner.journal_parts();
    let state = journal.project_state(&successor.project_id).unwrap();
    assert_eq!(state.owner, successor);
    assert!(state.owner_active);
}

#[cfg(target_os = "linux")]
#[test]
fn shutdown_cannot_retire_even_an_exited_managed_pane_without_its_runtime_drain() {
    let mut h = Harness::new();
    h.fake_backend();
    let fence = managed_launch::ManagedLaunchFence {
        owner: h.owner.app_identity().1.clone(), run_id: "run-1".into(), run_epoch: 1,
        attempt_id: uuid::Uuid::new_v4().to_string(), request_id: uuid::Uuid::new_v4().to_string(),
    };
    h.app.agents[0].managed_local = Some(managed_launch::ManagedRunSource::Reserved {
        fence, runnable_root: rudder_engine::protocol::ObjectRef::from_bytes(
            rudder_engine::protocol::ObjectKind::RunnableRoot, b"shutdown-test"),
    });
    let pane = h.app.agents[0].terminal.as_mut().unwrap();
    pane.request_termination();
    let deadline = Instant::now() + Duration::from_secs(3);
    while pane.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(shutdown_app(&mut h.app, "test managed shutdown").is_err());
    assert!(h.app.agents[0].terminal.is_some());
    h.owner.assert_current().unwrap();
}

#[test]
fn owner_screen_control_distinguishes_live_history_and_unknown_runs() {
    let mut h = Harness::new();
    h.start();
    h.fake_backend();
    let live = h.screen("run-1");
    assert_eq!(live["ok"], true);
    assert_eq!(live["status"]["terminal_screen"]["type"], "terminal-screen");
    assert_eq!(live["status"]["terminal_screen"]["screen"]["format"], "vt100-ansi-v1");
    assert_eq!(h.screen("unknown")["ok"], false);
    h.app.agents[0].status = AgentStatus::Paused;
    // A status flag alone cannot authorize an old mirror over a live PTY.
    assert_eq!(h.screen("run-1")["status"]["terminal_screen"]["type"], "terminal-screen");
    shutdown_app(&mut h.app, "test pause").unwrap();
    let history = h.screen("run-1");
    assert_eq!(history["ok"], true);
    assert_eq!(history["status"]["terminal_screen"]["type"], "terminal-history");
    assert_eq!(history["status"]["terminal_screen"]["runId"], "run-1");
    h.finish(Ok(())).unwrap();
}

#[test]
fn startup_binds_the_actual_owner_writer_and_excludes_duplicate_schedulers() {
    let mut h = Harness::new();
    let lifecycle = h.lifecycle.as_ref().unwrap();
    let fence = lifecycle.fence.clone();
    let writer = lifecycle.writer.as_ref().unwrap();
    assert_eq!(
        writer.project_state(&fence.project_id).unwrap().owner,
        fence
    );
    let (original, _) = h.owner.journal_parts();
    assert_eq!(
        writer.database_path().unwrap(),
        original.database_path().unwrap()
    );
    h.start();
    assert!(h.app.migration_resumes_attempted);
    assert_eq!(h.projection()["state"], "ready");
    for authority in [NativeAuthority::Fabric, NativeAuthority::Legacy] {
        assert!(EngineOwnership::acquire_in_home(&h.app.cwd, authority, &h.home).is_err());
    }
    assert!(h
        .lifecycle
        .as_mut()
        .unwrap()
        .start(&mut h.app, &mut h.owner)
        .is_err());
    h.finish(Ok(())).unwrap();
}

#[test]
fn multiple_surfaces_close_reopen_and_reconnect_without_replacing_live_work() {
    let mut h = Harness::new();
    h.start();
    let pid = h.fake_backend();
    let fence = h.owner.app_identity().1.clone();
    let first = h.surface();
    let second = h.surface();
    h.pump_until(|app| {
        app.attach_clients
            .iter()
            .filter(|client| client.dashboard.is_some())
            .count()
            == 2
    });
    let id = h.app.attach_clients[0].id;
    assert!(h.app.handle_view_event(
        id,
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
    ));
    h.app.close_attach_client(id, "surface closed");
    drop(first);
    drop(second);
    h.pump_until(|app| app.attach_clients.is_empty());
    assert_eq!(h.app.agents.len(), 1);
    let pane = h.app.agents[0].terminal.as_mut().unwrap();
    assert!(pane.try_wait().unwrap().is_none());
    assert_eq!(pane.child_process_id(), Some(pid));
    assert_eq!(h.app.agents[0].status, AgentStatus::Running);
    assert_eq!(h.owner.app_identity().1, &fence);
    let mut reconnected = h.surface();
    h.pump_until(|app| app.attach_clients.len() == 1 && app.attach_clients[0].dashboard.is_some());
    writeln!(
        reconnected,
        "{}",
        serde_json::to_string(&ClientMsg::Detach).unwrap()
    )
    .unwrap();
    h.pump_until(|app| app.attach_clients.is_empty());
    let mut pane_surface = UnixStream::connect(h.app.attach_socket_path()).unwrap();
    writeln!(
        pane_surface,
        "{}",
        serde_json::to_string(&ClientMsg::Attach {
            selector: "run-1".into(),
            rows: 30,
            cols: 100,
        })
        .unwrap()
    )
    .unwrap();
    h.pump_until(|app| {
        app.attach_clients.len() == 1 && app.attach_clients[0].run_id.as_deref() == Some("run-1")
    });
    assert_eq!(
        (h.app.attach_clients[0].rows, h.app.attach_clients[0].cols),
        (30, 100)
    );
    drop(pane_surface);
    h.pump_until(|app| app.attach_clients.is_empty());
    assert_eq!(
        h.app.agents[0]
            .terminal
            .as_ref()
            .unwrap()
            .child_process_id(),
        Some(pid)
    );
    h.finish(Ok(())).unwrap();
}

#[test]
fn explicit_engine_shutdown_observes_exit_persists_paused_and_removes_process() {
    let mut h = Harness::new();
    h.start();
    let attach_link = h.app.attach_socket_path();
    let attach_endpoint = fs::read_link(&attach_link).unwrap();
    let pid = h.fake_backend();
    assert_eq!(h.record()["process"]["pid"], pid);
    let fence = h.owner.app_identity().1.clone();
    h.finish(Ok(())).unwrap();
    assert!(fs::symlink_metadata(attach_link).is_err());
    assert!(fs::symlink_metadata(attach_endpoint).is_err());
    assert_eq!(h.app.agents[0].status, AgentStatus::Paused);
    assert!(h.app.agents[0].terminal.is_none());
    let record = h.record();
    assert_eq!(record["status"], "paused");
    assert!(record.get("process").is_none());
    assert_eq!(h.projection()["state"], "stopped");
    assert!(
        !h.owner
            .journal_parts()
            .0
            .project_state(&fence.project_id)
            .unwrap()
            .owner_active
    );
    assert_eq!(
        unsafe { libc::kill(pid as i32, 0) },
        -1,
        "direct child was actually reaped"
    );
}

#[test]
fn control_shutdown_is_an_acceptance_receipt_before_actual_app_retirement() {
    let mut h = Harness::new();
    h.start();
    h.fake_backend();
    let repository = h.app.cwd.clone();
    let (send, receive) = mpsc::sync_channel(1);
    let client = thread::spawn(move || {
        send.send(engine_status(&repository, true)).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if h.lifecycle
            .as_mut()
            .unwrap()
            .poll(&mut h.app, &mut h.owner)
            .unwrap()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "actual App did not accept shutdown control"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let receipt = receive
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    client.join().unwrap();
    assert_eq!(receipt["state"], "stopping");
    assert!(
        h.app.agents[0].terminal.is_some(),
        "acceptance is not process exit"
    );
    h.finish(Ok(())).unwrap();
    assert_eq!(h.record()["status"], "paused");
    assert!(h.record().get("process").is_none());
    assert_eq!(h.projection()["state"], "stopped");
}

#[test]
fn partial_startup_failure_stops_owned_work_and_records_uncertain_authority() {
    let mut h = Harness::new();
    let _existing_control = EngineControl::bind(&h.owner, None).unwrap();
    h.fake_backend();
    let startup = h
        .lifecycle
        .as_mut()
        .unwrap()
        .start(&mut h.app, &mut h.owner);
    assert!(
        startup.is_err(),
        "the second control socket must fail startup"
    );
    assert!(
        !h.app.migration_resumes_attempted,
        "bind failure must precede resume"
    );
    assert!(h.finish(startup.map(|_| ())).is_err());
    assert!(h.app.attach_server.is_none());
    assert!(h.app.agents[0].terminal.is_none());
    assert_eq!(h.record()["status"], "paused");
    assert!(h.record().get("process").is_none());
    assert_eq!(h.projection()["state"], "uncertain");
    let project = h.owner.app_identity().1.project_id.clone();
    assert!(
        h.owner
            .journal_parts()
            .0
            .project_state(&project)
            .unwrap()
            .owner_active
    );
}

#[test]
fn readiness_write_failure_revokes_admission_but_still_persists_owned_child_exit() {
    let mut h = Harness::new();
    h.fake_backend();
    let projection = h.app.cwd.join(".rudder/engine/owner.json");
    fs::remove_file(&projection).unwrap();
    fs::create_dir(&projection).unwrap();
    let startup = h
        .lifecycle
        .as_mut()
        .unwrap()
        .start(&mut h.app, &mut h.owner);
    assert!(startup.is_err());
    assert!(h.app.migration_resumes_attempted);
    assert!(h
        .lifecycle
        .as_ref()
        .unwrap()
        .lease
        .as_ref()
        .unwrap()
        .check_active_for_adoption()
        .is_err());
    assert!(h.finish(startup.map(|_| ())).is_err());
    assert!(h.app.attach_server.is_none());
    assert!(h.app.agents[0].terminal.is_none());
    assert_eq!(h.record()["status"], "paused");
    assert!(h.record().get("process").is_none());
    let project = h.owner.app_identity().1.project_id.clone();
    assert!(
        h.owner
            .journal_parts()
            .0
            .project_state(&project)
            .unwrap()
            .owner_active
    );
    assert!(projection.is_dir(), "no stopped projection was published");
}

#[test]
fn app_cannot_bind_a_different_checkout_or_resume_before_ingress() {
    let mut h = Harness::new();
    let original = h.app.cwd.clone();
    h.app.cwd = h.root.clone();
    assert!(h
        .lifecycle
        .as_mut()
        .unwrap()
        .start(&mut h.app, &mut h.owner)
        .is_err());
    assert!(h.app.attach_server.is_none());
    assert!(!h.app.migration_resumes_attempted);
    h.app.cwd = original;
    h.finish(Err(anyhow!("wrong checkout rejected")))
        .unwrap_err();
    assert_eq!(h.projection()["state"], "uncertain");
}

#[test]
fn attach_shutdown_retires_link_and_endpoint_independently() {
    for replace_link in [true, false] {
        let mut h = Harness::new();
        h.start();
        let link = h.app.attach_socket_path();
        let endpoint = fs::read_link(&link).unwrap();
        let successor_endpoint = if replace_link {
            // Keep the replacement as short as the actual endpoint on macOS.
            endpoint.with_extension("next")
        } else {
            fs::remove_file(&endpoint).unwrap();
            endpoint.clone()
        };
        let successor = UnixListener::bind(&successor_endpoint).unwrap();
        successor.set_nonblocking(true).unwrap();
        if replace_link {
            fs::rename(&link, h.root.join("original-attach-link")).unwrap();
            std::os::unix::fs::symlink(&successor_endpoint, &link).unwrap();
        }

        h.finish(Ok(())).unwrap();

        let connect_path = if replace_link {
            assert_eq!(fs::read_link(&link).unwrap(), successor_endpoint);
            assert!(fs::symlink_metadata(&endpoint).is_err());
            resolve_attach_socket(&link)
        } else {
            assert!(fs::symlink_metadata(&link).is_err());
            endpoint
        };
        let _client = UnixStream::connect(connect_path).unwrap();
        let _accepted = successor.accept().unwrap();
        drop(successor);
        fs::remove_file(successor_endpoint).unwrap();
    }
}

#[test]
fn owner_epoch_change_fences_writer_readiness_poll_and_shutdown_projection() {
    let mut h = Harness::new();
    h.start();
    h.fake_backend();
    let original_run = h.record();
    let control_path = h
        .lifecycle
        .as_ref()
        .unwrap()
        .control
        .as_ref()
        .unwrap()
        .path()
        .to_path_buf();
    fs::remove_file(&control_path).unwrap();
    let successor_control = UnixListener::bind(&control_path).unwrap();
    let attach_link = h.app.attach_socket_path();
    let attach_endpoint = fs::read_link(&attach_link).unwrap();
    fs::remove_file(&attach_endpoint).unwrap();
    let successor_attach = UnixListener::bind(&attach_endpoint).unwrap();
    successor_attach.set_nonblocking(true).unwrap();
    // Keep the original link allocated so this is a distinct object even on
    // filesystems that immediately reuse the inode of an unlinked symlink.
    fs::rename(&attach_link, h.root.join("original-attach-link")).unwrap();
    std::os::unix::fs::symlink(&attach_endpoint, &attach_link).unwrap();
    let lease = h
        .lifecycle
        .as_ref()
        .unwrap()
        .lease
        .as_ref()
        .unwrap()
        .clone();
    let (journal, fence) = h.owner.journal_parts();
    let successor = journal
        .transfer_owner(&fence.clone(), "successor", 100)
        .unwrap();
    let projection = b"{\"state\":\"successor-ready\"}";
    fs::write(h.app.cwd.join(".rudder/engine/owner.json"), projection).unwrap();
    assert!(
        lease.open_source_writer().is_err(),
        "actual epoch is checked before returning a writer"
    );
    assert!(h.owner.ready(None).is_err());
    assert!(h
        .lifecycle
        .as_mut()
        .unwrap()
        .poll(&mut h.app, &mut h.owner)
        .is_err());
    assert!(h.finish(Err(anyhow!("owner epoch changed"))).is_err());
    assert_eq!(fs::read_link(&attach_link).unwrap(), attach_endpoint);
    let mut client = UnixStream::connect(resolve_attach_socket(&attach_link)).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let (mut accepted, _) = successor_attach.accept().unwrap();
    accepted.write_all(b"successor").unwrap();
    let mut received = [0; 9];
    client.read_exact(&mut received).unwrap();
    assert_eq!(
        &received, b"successor",
        "stale teardown must preserve successor attach reachability"
    );
    drop(successor_attach);
    fs::remove_file(attach_link).unwrap();
    fs::remove_file(attach_endpoint).unwrap();
    assert!(
        control_path.exists(),
        "stale teardown must preserve the successor's control endpoint"
    );
    drop(successor_control);
    fs::remove_file(control_path).unwrap();
    assert_eq!(
        fs::read(h.app.cwd.join(".rudder/engine/owner.json")).unwrap(),
        projection
    );
    assert_eq!(
        h.owner
            .journal_parts()
            .0
            .project_state(&successor.project_id)
            .unwrap()
            .owner,
        successor
    );
    assert_eq!(
        h.record(),
        original_run,
        "stale cleanup cannot overwrite run projections"
    );
    assert!(
        h.app.agents[0].terminal.is_none(),
        "the original local PTY is still retired"
    );
}

#[test]
fn failed_paused_record_write_cannot_advertise_clean_engine_shutdown() {
    let mut h = Harness::new();
    h.start();
    h.fake_backend();
    let path = native_run_dir(&h.app.cwd, "run-1").join("run.json");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let error = h.finish(Err(anyhow!("original loop failure"))).unwrap_err();
    assert!(format!("{error:#}").contains("original loop failure"));
    assert!(format!("{error:#}").contains("could not persist paused engine state"));
    assert!(
        h.app.agents[0].terminal.is_none(),
        "confirmed exit still retires the pane"
    );
    assert_eq!(h.projection()["state"], "uncertain");
    let project = h.owner.app_identity().1.project_id.clone();
    assert!(
        h.owner
            .journal_parts()
            .0
            .project_state(&project)
            .unwrap()
            .owner_active
    );
}

#[test]
fn recovered_runtime_application_requires_app_custody_and_never_automatically_resumes() {
    use crate::engine_host::restart::{ExpectedRuntimeAttempt,NativeRestartRecovery,RecoveredRuntimeDisposition};
    let mut h=Harness::new();h.fake_backend();
    let attempt=ExpectedRuntimeAttempt {run_id:"run-1".into(),run_epoch:1,attempt_id:"original-attempt".into()};
    let mut recovery=NativeRestartRecovery {effects:rudder_engine::journal::Recovery {pending:vec![],uncertain:vec![]},
        runtimes:vec![RecoveredRuntimeDisposition::Paused(attempt.clone())]};
    assert!(apply_recovered_runtimes_on_app(&mut h.app,&recovery).is_err());
    assert!(h.app.agents[0].terminal.is_some(),"recovery must retain actual live terminal custody");
    shutdown_app(&mut h.app,"test original retirement").unwrap();
    h.app.agents[0].status=AgentStatus::Running;
    h.app.pending_migration_resumes.push(MigratedAgent {run_id:"run-1".into(),session_id:"prior-session".into(),workspace_path:h.app.cwd.clone(),fresh_prompt:None});
    apply_recovered_runtimes_on_app(&mut h.app,&recovery).unwrap();
    assert_eq!(h.record()["status"],"paused");assert!(h.record().get("process").is_none());
    assert!(h.app.pending_migration_resumes.is_empty());
    assert!(h.app.restart_home_boot_suppressed.contains("run-1"));
    assert!(!h.app.focus_resume_attempted.contains("run-1"),"Paused still permits an explicit resume attempt");
    h.app.home_runtime=true;
    h.app.reopen_live_rows_after_home_boot();
    assert!(h.app.agents[0].terminal.is_none(),"home boot cannot resume recovered rows");
    h.app.agents[0].status=AgentStatus::Merged;
    apply_recovered_runtimes_on_app(&mut h.app,&recovery).unwrap();
    assert_eq!(h.record()["status"],"merged","process retirement cannot erase accepted integration");
    recovery.runtimes=vec![RecoveredRuntimeDisposition::Quarantined {attempt,
        reason:crate::engine_host::restart::RuntimeQuarantineReason::UncertainCustody}];
    apply_recovered_runtimes_on_app(&mut h.app,&recovery).unwrap();
    assert_eq!(h.record()["status"],"merged","quarantine preserves accepted integration");
    h.app.agents[0].status=AgentStatus::Running;
    apply_recovered_runtimes_on_app(&mut h.app,&recovery).unwrap();
    assert_eq!(h.record()["status"],"orphaned");assert!(h.app.agents[0].terminal.is_none());
    assert!(!h.app.reopen_conversation_at(0));
    assert!(h.app.notice.as_deref().unwrap().contains("requires explicit reconciliation"));
    h.finish(Ok(())).unwrap();
}
