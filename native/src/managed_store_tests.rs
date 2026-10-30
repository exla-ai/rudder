use super::*;
use rudder_engine::{
    journal::OwnerFence,
    protocol::{ObjectKind, ObjectRef},
};

pub(crate) struct Fixture {
    _root: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) repo: PathBuf,
    pub(crate) journal: Journal,
    pub(crate) owner: OwnerFence,
    pub(crate) request: ManagedProjectPrepare,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("rudder-managed-store-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let path = root.canonicalize().unwrap();
        let home = path.join("home");
        let repo = path.join("repo");
        fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(home.join("fabric"))
            .unwrap();
        fs::create_dir(&repo).unwrap();
        let mut journal = Journal::open(home.join("fabric/journal.sqlite")).unwrap();
        let project = rudder_engine::paths::local_project_id(&repo).unwrap();
        let owner = journal
            .create_project(&project, "owner", "base", 0)
            .unwrap();
        let request = ManagedProjectPrepare {
            request_id: uuid::Uuid::new_v4().to_string(),
            namespace: namespace(&home, &repo, &project, &journal).unwrap(),
        };
        Self {
            _root: root,
            home,
            repo,
            journal,
            owner,
            request,
        }
    }
    pub(crate) fn prepare(&mut self) -> ManagedStore {
        persist_prepare_intent(&self.request, &self.journal).unwrap();
        let receipt = self
            .journal
            .prepare_managed_project(&self.owner, &self.request, 1)
            .unwrap();
        persist_prepared_receipt(&receipt, &self.journal).unwrap()
    }
    pub(crate) fn source(&mut self, run: &str) -> ManagedRunSource {
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let run_epoch = self
            .journal
            .observe_run_attempt(&self.owner, run, &attempt_id, 2)
            .unwrap();
        ManagedRunSource::Reserved {
            fence: ManagedLaunchFence {
                owner: self.owner.clone(),
                run_id: run.into(),
                run_epoch,
                attempt_id,
                request_id: uuid::Uuid::new_v4().to_string(),
            },
            runnable_root: ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"retained"),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self._root);
    }
}
pub(crate) fn assignment(source: &ManagedRunSource) -> WorkspaceAssignment {
    let ManagedRunSource::Reserved {
        fence,
        runnable_root,
    } = source
    else {
        panic!("fixture reservation required");
    };
    WorkspaceAssignment {
        format_version: 1,
        fence: fence.clone(),
        runnable_root: runnable_root.clone(),
        source: ObjectRef::from_bytes(ObjectKind::SourceRoot, b"source"),
        seed: ObjectRef::from_bytes(ObjectKind::Directory, b"seed"),
        observation: rudder_engine::vfs::rpc::ObservationBinding {
            format_version: 1,
            attempt: rudder_engine::vfs::rpc::AttemptContext {
                execution_id: fence.attempt_id.clone(),
                operation_id: uuid::Uuid::new_v4().to_string(),
            },
            view_id: uuid::Uuid::new_v4().to_string(),
            owner: fence.owner_epoch(),
            namespace: uuid::Uuid::new_v4().to_string(),
            profile: ObjectRef::from_bytes(ObjectKind::Blob, b"profile"),
            exclusion_policy_digest: "a".repeat(64),
        },
        host_context: rudder_engine::execution::ProtectedHostContextRef {
            format_version: 1,
            path: "/private/context.json".into(),
            sha256: "b".repeat(64),
            size_bytes: 1,
        },
    }
}

#[test]
fn managed_store_retains_private_immutable_reservations_outside_legacy_discovery() {
    let mut fixture = Fixture::new();
    let store = fixture.prepare();
    let location = store.run("run").unwrap();
    let source = fixture.source("run");
    location.persist_reservation(&source).unwrap();
    location.persist_reservation(&source).unwrap();
    assert!(location
        .directory()
        .starts_with(fixture.home.join("fabric/managed-projects")));
    assert!(!fixture.repo.join(".rudder").exists());
    assert!(!fixture.home.join("fabric/workspace-assignments").exists());
    let mut changed = source.clone();
    if let ManagedRunSource::Reserved { fence, .. } = &mut changed {
        fence.request_id = uuid::Uuid::new_v4().to_string();
    }
    assert!(location.persist_reservation(&changed).is_err());
    let recovered = store.recover_runs().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].source, source);
}

#[test]
fn managed_store_prepare_crash_phases_fail_closed_without_implicit_repair() {
    let mut fixture = Fixture::new();
    persist_prepare_intent(&fixture.request, &fixture.journal).unwrap();
    assert!(has_namespace_evidence(&fixture.home, &fixture.owner.project_id).unwrap());
    assert!(ManagedStore::open(&fixture.journal, &fixture.request.namespace).is_err());
    assert!(fixture
        .journal
        .managed_project(&fixture.owner.project_id)
        .unwrap()
        .is_none());
    let receipt = fixture
        .journal
        .prepare_managed_project(&fixture.owner, &fixture.request, 1)
        .unwrap();
    assert!(ManagedStore::open(&fixture.journal, &fixture.request.namespace).is_err());
    let store = persist_prepared_receipt(&receipt, &fixture.journal).unwrap();
    let next = fixture
        .journal
        .transfer_owner(&fixture.owner, "new-owner", 2)
        .unwrap();
    let historical = ManagedStore::open(&fixture.journal, &fixture.request.namespace).unwrap();
    assert_eq!(historical, store);
    assert_eq!(historical.receipt().prepared_owner, fixture.owner);
    assert_ne!(historical.receipt().prepared_owner, next);
    fs::remove_file(store.binding().directory.join("project.json")).unwrap();
    assert!(ManagedStore::open(&fixture.journal, store.binding()).is_err());
    assert!(has_namespace_evidence(&fixture.home, &fixture.owner.project_id).unwrap());
}

#[test]
fn managed_store_assignment_requires_the_exact_durable_reservation() {
    let mut fixture = Fixture::new();
    let store = fixture.prepare();
    let location = store.run("run").unwrap();
    let source = fixture.source("run");
    let assigned = assignment(&source);
    assert!(location.persist_assignment(&assigned).is_err());
    location.persist_reservation(&source).unwrap();
    location.persist_assignment(&assigned).unwrap();
    location.persist_assignment(&assigned).unwrap();
    let mut changed = assigned.clone();
    changed.observation.namespace = uuid::Uuid::new_v4().to_string();
    assert!(location.persist_assignment(&changed).is_err());
    assert!(store
        .run("other")
        .unwrap()
        .persist_assignment(&assigned)
        .is_err());
    let recovered = store.recover_runs().unwrap();
    assert_eq!(
        recovered[0].source,
        ManagedRunSource::Assigned {
            assignment: Box::new(assigned)
        }
    );
}

#[test]
fn managed_store_missing_assignment_or_corrupt_projection_stays_visible_and_quarantined() {
    let mut fixture = Fixture::new();
    let store = fixture.prepare();
    let location = store.run("run").unwrap();
    let source = fixture.source("run");
    let assigned = assignment(&source);
    location.persist_reservation(&source).unwrap();
    location.persist_assignment(&assigned).unwrap();
    let assigned = ManagedRunSource::Assigned {
        assignment: Box::new(assigned),
    };
    location
        .write_projection(&assigned, &serde_json::json!({"task":"private recovery"}))
        .unwrap();
    fs::remove_file(
        location
            .attempt_directory(&source.fence().unwrap().attempt_id)
            .unwrap()
            .join("assignment.json"),
    )
    .unwrap();
    let recovered = store.recover_runs().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].source, ManagedRunSource::Quarantined);
    fs::write(location.directory().join("projection.json"), b"malformed").unwrap();
    assert_eq!(
        store.recover_runs().unwrap()[0].source,
        ManagedRunSource::Quarantined
    );
    fs::remove_file(location.directory().join("identity.json")).unwrap();
    assert_eq!(
        store.recover_runs().unwrap()[0].source,
        ManagedRunSource::Quarantined
    );
    assert_eq!(store.recover_runs().unwrap()[0].run_id, "run");
}

#[test]
fn managed_store_refuses_symlinks_nonprivate_modes_and_wrong_identity() {
    let mut fixture = Fixture::new();
    let store = fixture.prepare();
    let location = store.run("run").unwrap();
    let source = fixture.source("run");
    location.persist_reservation(&source).unwrap();
    let path = location
        .attempt_directory(&source.fence().unwrap().attempt_id)
        .unwrap()
        .join("reservation.json");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        store.recover_runs().unwrap()[0].source,
        ManagedRunSource::Quarantined
    );
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("missing", &path).unwrap();
    assert_eq!(
        store.recover_runs().unwrap()[0].source,
        ManagedRunSource::Quarantined
    );
    let mut wrong = store.binding().clone();
    wrong.home = fixture.repo.clone();
    assert!(ManagedStore::open(&fixture.journal, &wrong).is_err());
    let mut wrong = store.binding().clone();
    wrong.project_id = "elsewhere".into();
    assert!(ManagedStore::open(&fixture.journal, &wrong).is_err());
    fs::remove_file(store.binding().directory.join("prepared.json")).unwrap();
    std::os::unix::fs::symlink("missing", store.binding().directory.join("prepared.json")).unwrap();
    assert!(ManagedStore::open(&fixture.journal, store.binding()).is_err());
}

#[test]
fn managed_store_recovery_loader_and_save_never_consult_or_create_legacy_run_or_graph() {
    let mut fixture = Fixture::new();
    let store = fixture.prepare();
    let location = store.run("run").unwrap();
    let source = fixture.source("run");
    location.persist_reservation(&source).unwrap();
    location.write_projection(&source,&serde_json::json!({"task":"recover me","session":{"nativeSessionId":"old"},"workspace":{"path":"/wrong","jjChangeId":"old"},"nodeId":"old-node"})).unwrap();
    let mut runs = crate::gitio::load_managed_agents(&store).unwrap();
    assert_eq!(runs.len(), 1);
    let run = &mut runs[0];
    assert_eq!(run.status, crate::AgentStatus::Orphaned);
    assert!(run.session_id.is_none());
    assert!(run.workspace_path.is_none());
    assert!(run.jj_change_id.is_none());
    assert!(run.node_id.is_none());
    assert!(!crate::launch::can_resume_agent(run));
    assert_eq!(run.managed_store.as_ref(), Some(&location));
    crate::gitio::save_native_run_record(&fixture.repo, run).unwrap();
    assert!(!fixture.repo.join(".rudder").exists());
    let mut app = crate::App::new();
    app.agents = runs;
    app.agents[0].node_id = Some("never-legacy-graph".into());
    assert_eq!(
        app.build_mirror_payload()["nodes"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn managed_store_crash_before_reservation_files_recovers_original_ids_only_as_quarantine() {
    let mut fixture = Fixture::new();
    let store = fixture.prepare();
    let source = fixture.source("no-files-yet");
    let recovered = store.recover_runs().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].run_id, "no-files-yet");
    assert_eq!(recovered[0].source, ManagedRunSource::Quarantined);
    assert!(recovered[0].location.is_none());
    assert!(recovered[0]
        .diagnostic
        .as_ref()
        .unwrap()
        .contains(&source.fence().unwrap().attempt_id));
    let rows = crate::gitio::load_managed_agents(&store).unwrap();
    assert!(rows[0].session_id.is_none());
    assert_eq!(rows[0].status, crate::AgentStatus::Orphaned);
    assert!(!fixture.repo.join(".rudder").exists());
}

#[test]
fn managed_store_exhausted_or_malformed_journal_scan_is_visible_and_quarantines_all_results() {
    let mut fixture = Fixture::new();
    let store = fixture.prepare();
    for index in 0..129 {
        fixture.source(&format!("run-{index}"));
    }
    let mut rows = Vec::new();
    store.recover_journal(&mut rows, 1).unwrap();
    assert_eq!(rows.len(), 129);
    assert!(rows
        .iter()
        .all(|row| row.source == ManagedRunSource::Quarantined));
    assert!(rows.iter().any(|row| row
        .diagnostic
        .as_deref()
        .is_some_and(|reason| reason.contains("budget exhausted"))));
    assert_eq!(store.recover_runs().unwrap().len(), 129);
    let database = rusqlite::Connection::open(fixture.journal.database_path().unwrap()).unwrap();
    database
        .execute(
            "UPDATE events SET payload_json='{}' WHERE kind='run-attempt-observed'",
            [],
        )
        .unwrap();
    let rows = store.recover_runs().unwrap();
    assert!(rows.iter().any(|row| row
        .diagnostic
        .as_deref()
        .is_some_and(|reason| reason.contains("malformed"))));
    assert!(rows
        .iter()
        .all(|row| row.source == ManagedRunSource::Quarantined));
}

#[test]
fn managed_store_known_row_refuses_legacy_adopt_before_replacement_or_resume() {
    let mut fixture = Fixture::new();
    let store = fixture.prepare();
    let source = fixture.source("run");
    let location = store.run("run").unwrap();
    location.persist_reservation(&source).unwrap();
    let mut app = crate::App::new();
    app.cwd = fixture.repo.clone();
    app.agents = crate::gitio::load_managed_agents(&store).unwrap();
    let old = fixture.repo.join(".rudder/runs/run");
    fs::create_dir_all(&old).unwrap();
    // An invalid managed marker also prevents any provider spawn if the
    // pre-lookup regression returns; the replacement itself must be refused.
    let staged = serde_json::to_vec(
        &serde_json::json!({"id":"run","task":"must never replace owned row","managedLocal":null}),
    )
    .unwrap();
    fs::write(old.join("run.json"), &staged).unwrap();
    assert!(
        matches!(app.deliver_adopt_request("run"),crate::SteerDelivery::Failed(reason) if reason.contains("managed"))
    );
    assert_eq!(app.agents.len(), 1);
    assert_eq!(app.agents[0].managed_local, Some(source));
    assert_eq!(app.agents[0].managed_store.as_ref(), Some(&location));
    assert!(app.agents[0].terminal.is_none());
    assert_eq!(fs::read(old.join("run.json")).unwrap(), staged);
}

#[test]
fn managed_store_ui_delete_retains_private_and_experimental_recovery_evidence() {
    for experimental in [false, true] {
        let mut fixture = Fixture::new();
        let store = fixture.prepare();
        let run_id = uuid::Uuid::new_v4().to_string();
        let source = fixture.source(&run_id);
        let location = store.run(&run_id).unwrap();
        location.persist_reservation(&source).unwrap();
        let old = fixture.repo.join(".rudder/runs").join(&run_id);
        fs::create_dir_all(&old).unwrap();
        let original = serde_json::to_vec(
            &serde_json::json!({"id":run_id,"task":"retained old source","managedLocal":null}),
        )
        .unwrap();
        fs::write(old.join("run.json"), &original).unwrap();
        let mut app = crate::App::new();
        app.cwd = fixture.repo.clone();
        app.agents = if experimental {
            vec![crate::gitio::agent_from_run_record(
                &fixture.repo,
                serde_json::from_slice(&original).unwrap(),
            )
            .unwrap()]
        } else {
            crate::gitio::load_managed_agents(&store).unwrap()
        };
        let notice = app.delete_agent_now(0);
        assert!(notice.contains("retained") && notice.contains("recovery"));
        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.agents[0].id, run_id);
        assert_eq!(fs::read(old.join("run.json")).unwrap(), original);
        assert!(location.directory().join("identity.json").exists());
        assert_eq!(
            location
                .reservation(&source.fence().unwrap().attempt_id)
                .unwrap(),
            Some(source)
        );
    }
}

#[test]
fn managed_store_screen_watch_never_projects_managed_pty_bytes_into_legacy_store() {
    use std::time::{Duration, Instant};
    let mut fixture = Fixture::new();
    let store = fixture.prepare();
    let source = fixture.source("run");
    store
        .run("run")
        .unwrap()
        .persist_reservation(&source)
        .unwrap();
    let old = fixture.repo.join(".rudder/runs/run");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("screen.watch"), b"fixture watcher").unwrap();
    let command = crate::TerminalCommand::with_args(
        "/usr/bin/env",
        [
            "-i",
            "PATH=/usr/bin:/bin",
            "TERM=xterm-256color",
            "/bin/sh",
            "-c",
            "printf managed_fixture_screen; sleep 0.05",
        ],
    );
    let mut pane = crate::TerminalPane::spawn_shell_or_command(
        Some(command),
        crate::TerminalPaneOptions {
            cwd: Some(fixture.repo.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        pane.drain_output();
        if pane
            .visible_lines_snapshot()
            .join("\n")
            .contains("managed_fixture_screen")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture PTY did not produce its static screen"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut app = crate::App::new();
    app.cwd = fixture.repo.clone();
    app.agents = crate::gitio::load_managed_agents(&store).unwrap();
    app.agents[0].terminal = Some(pane);
    app.write_watched_screens();
    assert!(!old.join("screen.txt").exists());
    // Same real watcher/screen still works for ordinary legacy rows.
    app.agents[0].managed_local = None;
    app.agents[0].managed_store = None;
    app.write_watched_screens();
    assert!(fs::read_to_string(old.join("screen.txt"))
        .unwrap()
        .contains("managed_fixture_screen"));
}
