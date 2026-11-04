use rudder_engine::journal::{Journal, ManagedProjectNamespace, ManagedProjectPrepare, OwnerFence};

struct Fixture {
    _root: tempfile::TempDir,
    journal: Journal,
    owner: OwnerFence,
    request: ManagedProjectPrepare,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap().join("home");
        let repository = root.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(home.join("fabric")).unwrap();
        std::fs::create_dir(&repository).unwrap();
        let path = home.join("fabric/journal.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let owner = journal
            .create_project("project-a", "owner-a", "base", 0)
            .unwrap();
        let request = ManagedProjectPrepare {
            request_id: uuid::Uuid::new_v4().to_string(),
            namespace: ManagedProjectNamespace {
                format_version: 1,
                project_id: owner.project_id.clone(),
                directory: ManagedProjectNamespace::directory_for(&home, &owner.project_id),
                home,
                journal: path,
                repository,
            },
        };
        Self {
            _root: root,
            journal,
            owner,
            request,
        }
    }
    fn version(&self) -> u32 {
        rusqlite::Connection::open(&self.request.namespace.journal)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }
}

#[test]
fn ordinary_open_and_floor_lookup_leave_schema_four_unchanged() {
    let fixture = Fixture::new();
    assert_eq!(fixture.version(), 4);
    assert_eq!(fixture.journal.managed_project("project-a").unwrap(), None);
    assert_eq!(fixture.version(), 4);
    let opened = Journal::open(&fixture.request.namespace.journal).unwrap();
    assert!(opened.managed_project("project-a").unwrap().is_none());
    assert_eq!(fixture.version(), 4);
}

#[test]
fn explicit_prepare_is_durable_exactly_idempotent_and_project_scoped() {
    let mut fixture = Fixture::new();
    let receipt = fixture
        .journal
        .prepare_managed_project(&fixture.owner, &fixture.request, 10)
        .unwrap();
    assert_eq!(fixture.version(), 5);
    let sequence = fixture.journal.project_state("project-a").unwrap().sequence;
    assert_eq!(
        fixture
            .journal
            .prepare_managed_project(&fixture.owner, &fixture.request, 20)
            .unwrap(),
        receipt
    );
    assert_eq!(
        fixture.journal.project_state("project-a").unwrap().sequence,
        sequence
    );
    assert_eq!(
        Journal::open(&fixture.request.namespace.journal)
            .unwrap()
            .managed_project("project-a")
            .unwrap(),
        Some(receipt)
    );
    let other = fixture
        .journal
        .create_project("project-b", "owner-b", "base", 11)
        .unwrap();
    assert!(fixture
        .journal
        .managed_project(&other.project_id)
        .unwrap()
        .is_none());
    assert!(fixture
        .journal
        .register_run(&other, "ordinary-run", 12)
        .is_ok());
}

#[test]
fn failed_prepare_rolls_back_schema_floor_and_event_in_one_transaction() {
    let mut fixture = Fixture::new();
    let database = rusqlite::Connection::open(&fixture.request.namespace.journal).unwrap();
    database.execute_batch("CREATE TRIGGER reject_managed_prepare BEFORE INSERT ON events WHEN NEW.kind='managed-project-prepared' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    let sequence = fixture.journal.project_state("project-a").unwrap().sequence;
    assert!(fixture
        .journal
        .prepare_managed_project(&fixture.owner, &fixture.request, 10)
        .is_err());
    assert_eq!(fixture.version(), 4);
    assert_eq!(
        fixture.journal.project_state("project-a").unwrap().sequence,
        sequence
    );
    assert!(fixture
        .journal
        .managed_project("project-a")
        .unwrap()
        .is_none());
    assert_eq!(
        database
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='managed_projects'",
                [],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn stale_or_inactive_owner_cannot_install_a_floor() {
    let mut fixture = Fixture::new();
    let stale = fixture.owner.clone();
    fixture.owner = fixture
        .journal
        .transfer_owner(&stale, "owner-b", 1)
        .unwrap();
    assert!(fixture
        .journal
        .prepare_managed_project(&stale, &fixture.request, 10)
        .is_err());
    assert_eq!(fixture.version(), 4);
    fixture.journal.release_owner(&fixture.owner, 2).unwrap();
    assert!(fixture
        .journal
        .prepare_managed_project(&fixture.owner, &fixture.request, 10)
        .is_err());
    assert_eq!(fixture.version(), 4);
}

#[test]
fn prepared_receipt_survives_restart_without_reviving_historical_owner() {
    let mut fixture = Fixture::new();
    let receipt = fixture
        .journal
        .prepare_managed_project(&fixture.owner, &fixture.request, 10)
        .unwrap();
    let old = fixture.owner.clone();
    let next = fixture
        .journal
        .transfer_owner(&old, "new-engine", 20)
        .unwrap();
    let mut reopened = Journal::open(&fixture.request.namespace.journal).unwrap();
    assert_eq!(
        reopened.managed_project("project-a").unwrap(),
        Some(receipt.clone())
    );
    assert_eq!(reopened.project_state("project-a").unwrap().owner, next);
    assert!(reopened
        .prepare_managed_project(&old, &fixture.request, 30)
        .is_err());
    assert_eq!(
        reopened
            .prepare_managed_project(&next, &fixture.request, 30)
            .unwrap(),
        receipt
    );
    assert_eq!(reopened.project_state("project-a").unwrap().owner, next);
}

#[test]
fn prepare_refuses_changed_request_home_project_or_database_identity() {
    let mut fixture = Fixture::new();
    let mut wrong = fixture.request.clone();
    wrong.namespace.project_id = "other".into();
    wrong.namespace.directory =
        ManagedProjectNamespace::directory_for(&wrong.namespace.home, "other");
    assert!(fixture
        .journal
        .prepare_managed_project(&fixture.owner, &wrong, 1)
        .is_err());
    let mut wrong = fixture.request.clone();
    wrong.namespace.home = wrong.namespace.home.join("other-home");
    wrong.namespace.directory =
        ManagedProjectNamespace::directory_for(&wrong.namespace.home, "project-a");
    wrong.namespace.journal = wrong.namespace.home.join("fabric/journal.sqlite");
    assert!(fixture
        .journal
        .prepare_managed_project(&fixture.owner, &wrong, 1)
        .is_err());
    assert_eq!(fixture.version(), 4);
    fixture
        .journal
        .prepare_managed_project(&fixture.owner, &fixture.request, 1)
        .unwrap();
    let mut changed = fixture.request.clone();
    changed.request_id = uuid::Uuid::new_v4().to_string();
    assert!(fixture
        .journal
        .prepare_managed_project(&fixture.owner, &changed, 2)
        .is_err());
    let mut changed = fixture.request.clone();
    changed.namespace.repository = changed.namespace.repository.join("elsewhere");
    assert!(fixture
        .journal
        .prepare_managed_project(&fixture.owner, &changed, 2)
        .is_err());
}

#[test]
fn corrupt_or_future_floor_is_not_reported_as_absent() {
    let mut fixture = Fixture::new();
    fixture
        .journal
        .prepare_managed_project(&fixture.owner, &fixture.request, 1)
        .unwrap();
    let database = rusqlite::Connection::open(&fixture.request.namespace.journal).unwrap();
    database
        .execute("UPDATE managed_projects SET minimum_writer_protocol=2", [])
        .unwrap();
    assert!(fixture.journal.managed_project("project-a").is_err());
    database
        .execute(
            "UPDATE managed_projects SET minimum_writer_protocol=1,receipt_json='{}'",
            [],
        )
        .unwrap();
    assert!(fixture.journal.managed_project("project-a").is_err());
    database.execute_batch("PRAGMA user_version=6").unwrap();
    assert!(Journal::open(&fixture.request.namespace.journal).is_err());
}

#[test]
fn recovery_attempt_pages_ignore_pre_floor_events_and_do_not_revive_old_owner() {
    use rudder_engine::journal::ManagedAttemptRecovery;
    let mut fixture = Fixture::new();
    fixture
        .journal
        .observe_run_attempt(
            &fixture.owner,
            "legacy-before-floor",
            &uuid::Uuid::new_v4().to_string(),
            1,
        )
        .unwrap();
    let receipt = fixture
        .journal
        .prepare_managed_project(&fixture.owner, &fixture.request, 2)
        .unwrap();
    let first = uuid::Uuid::new_v4().to_string();
    let second = uuid::Uuid::new_v4().to_string();
    fixture
        .journal
        .observe_run_attempt(&fixture.owner, "first", &first, 3)
        .unwrap();
    fixture
        .journal
        .observe_run_attempt(&fixture.owner, "second", &second, 4)
        .unwrap();
    let next = fixture
        .journal
        .transfer_owner(&fixture.owner, "new-owner", 5)
        .unwrap();
    let sequence = fixture.journal.project_state("project-a").unwrap().sequence;
    let page = fixture
        .journal
        .managed_recovery_attempts(&receipt, None, 1)
        .unwrap();
    assert!(
        matches!(&page.observations[..],[ManagedAttemptRecovery::Observed {run_id,attempt_id,run_epoch:1,..}] if run_id=="first" && attempt_id==&first)
    );
    let page = fixture
        .journal
        .managed_recovery_attempts(&receipt, page.next.as_ref(), 1)
        .unwrap();
    assert!(
        matches!(&page.observations[..],[ManagedAttemptRecovery::Observed {run_id,attempt_id,run_epoch:1,..}] if run_id=="second" && attempt_id==&second)
    );
    assert!(page.next.is_none());
    assert_eq!(
        fixture.journal.project_state("project-a").unwrap().owner,
        next
    );
    assert_eq!(
        fixture.journal.project_state("project-a").unwrap().sequence,
        sequence
    );
    let mut fake = receipt.clone();
    fake.sequence += 1;
    assert!(fixture
        .journal
        .managed_recovery_attempts(&fake, None, 1)
        .is_err());
    assert!(fixture
        .journal
        .managed_recovery_attempts(&receipt, None, 0)
        .is_err());
    assert!(fixture
        .journal
        .managed_recovery_attempts(&receipt, None, 257)
        .is_err());
}

#[test]
fn recovery_cursor_is_receipt_scoped_and_malformed_events_stay_explicit() {
    use rudder_engine::journal::ManagedAttemptRecovery;
    let mut fixture = Fixture::new();
    let receipt = fixture
        .journal
        .prepare_managed_project(&fixture.owner, &fixture.request, 1)
        .unwrap();
    for run in ["first", "second", "third"] {
        fixture
            .journal
            .observe_run_attempt(&fixture.owner, run, &uuid::Uuid::new_v4().to_string(), 2)
            .unwrap();
    }
    let page = fixture
        .journal
        .managed_recovery_attempts(&receipt, None, 1)
        .unwrap();
    let cursor = page.next.unwrap();
    let other = fixture
        .journal
        .create_project("project-b", "other", "base", 3)
        .unwrap();
    let mut request = fixture.request.clone();
    request.request_id = uuid::Uuid::new_v4().to_string();
    request.namespace.project_id = other.project_id.clone();
    request.namespace.directory =
        ManagedProjectNamespace::directory_for(&request.namespace.home, &other.project_id);
    let other_receipt = fixture
        .journal
        .prepare_managed_project(&other, &request, 4)
        .unwrap();
    assert!(fixture
        .journal
        .managed_recovery_attempts(&other_receipt, Some(&cursor), 1)
        .is_err());
    let database = rusqlite::Connection::open(&fixture.request.namespace.journal).unwrap();
    database.execute("UPDATE events SET payload_json=?1 WHERE project_id='project-a' AND kind='run-attempt-observed' AND sequence=(SELECT max(sequence) FROM events WHERE project_id='project-a' AND kind='run-attempt-observed')",["x".repeat(8193)]).unwrap();
    let page = fixture
        .journal
        .managed_recovery_attempts(&receipt, Some(&cursor), 2)
        .unwrap();
    assert_eq!(page.observations.len(), 2);
    assert!(matches!(
        page.observations[1],
        ManagedAttemptRecovery::Malformed { .. }
    ));
    assert!(page.next.is_none());
}
