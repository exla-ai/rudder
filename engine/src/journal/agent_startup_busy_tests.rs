//! Real SQLite contention only. These fixtures issue ordinary local agent
//! capabilities; they do not construct a native broker, lease or fresh owner.
use super::*;
use crate::agent_api::{AgentApi, AgentIdentityFile, AGENT_STARTUP_BUSY_TIMEOUT};
use crate::journal::{CapabilityGrant, ManagedProjectNamespace, ManagedProjectPrepare, OwnerFence};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc;

struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
    identity_path: PathBuf,
    home: PathBuf,
    repository: PathBuf,
    owner: OwnerFence,
    journal: Option<Journal>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let home = root.join("home");
        let repository = root.join("repository");
        std::fs::create_dir_all(home.join("fabric")).unwrap();
        std::fs::create_dir(&repository).unwrap();
        let path = home.join("fabric/journal.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let owner = journal.create_project("startup-project", "engine", "base", 0).unwrap();
        journal.register_run(&owner, "run", 0).unwrap();
        let grant = CapabilityGrant {
            principal: "agent:run".into(),
            run_id: "run".into(),
            run_epoch: 1,
            operations: BTreeSet::from(["agents:find".into()]),
            recipients: BTreeMap::new(),
            scopes: BTreeSet::new(),
            topics: BTreeSet::new(),
            expires_at_ms: None,
        };
        let token = journal.issue_capability(&owner, &grant, 0).unwrap();
        let identity = AgentIdentityFile {
            format_version: 1,
            project_id: owner.project_id.clone(),
            run_id: "run".into(),
            run_epoch: 1,
            owner_epoch: owner.epoch,
            token,
        };
        let identity_path = root.join("identity.json");
        std::fs::write(&identity_path, serde_json::to_vec(&identity).unwrap()).unwrap();
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&identity_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        Self { _directory: directory, path, identity_path, home, repository, owner, journal: Some(journal) }
    }

    fn take_exclusive(&mut self) -> Journal {
        let journal = self.journal.take().unwrap();
        journal.conn.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;").unwrap();
        journal
    }

    fn add_real_managed_marker(&mut self) {
        let request = ManagedProjectPrepare {
            request_id: uuid::Uuid::new_v4().to_string(),
            namespace: ManagedProjectNamespace {
                format_version: 1,
                project_id: self.owner.project_id.clone(),
                home: self.home.clone(),
                journal: self.path.clone(),
                repository: self.repository.clone(),
                directory: ManagedProjectNamespace::directory_for(&self.home, &self.owner.project_id),
            },
        };
        self.journal.as_mut().unwrap().prepare_managed_project(&self.owner, &request, 0).unwrap();
        let marker = self.repository.join(".rudder/engine/fresh-project.json");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(marker, b"{}\n").unwrap();
    }
}

fn release_after(locker: Journal, delay: Duration, commit: bool) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        locker.conn.execute_batch(if commit { "COMMIT" } else { "ROLLBACK" }).unwrap();
        // EXCLUSIVE locking mode retains its lock past COMMIT/ROLLBACK until
        // the actual connection closes. Never use the transaction event alone.
        drop(locker);
    })
}

#[test]
fn startup_busy_read_observes_real_sqlite_lock_then_original_release() {
    let mut fixture = Fixture::new();
    let locker = fixture.take_exclusive();
    let path = fixture.path.clone();
    let (observed, receive) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        let journal = Journal { conn };
        let deadline = Instant::now() + AGENT_STARTUP_BUSY_TIMEOUT;
        let mut reported = false;
        let receipt = retry_startup_read(Some(deadline), || {
            let result = journal.managed_project("startup-project");
            if !reported && result.as_ref().err().is_some_and(|e| startup_sqlite_busy(e)) {
                reported = true;
                observed.send(()).unwrap();
            }
            result
        });
        (receipt, reported, Instant::now() < deadline)
    });
    receive.recv_timeout(Duration::from_secs(2)).expect("reader must encounter the actual exclusive lock");
    locker.conn.execute_batch("ROLLBACK").unwrap();
    drop(locker);
    let (receipt, reported, timely) = reader.join().unwrap();
    assert!(reported);
    assert!(receipt.unwrap().is_none());
    assert!(timely, "read was admitted beyond its original startup window");
}

#[test]
fn startup_busy_agent_api_authenticates_after_real_lock_release() {
    let mut fixture = Fixture::new();
    let locker = fixture.take_exclusive();
    let release = release_after(locker, Duration::from_millis(10), false);
    let result = AgentApi::open(&fixture.path, &fixture.identity_path);
    release.join().unwrap();
    let api = result.unwrap();
    let actual = api.authenticated_identity().unwrap();
    assert_eq!(actual.project_id, fixture.owner.project_id);
    assert_eq!(actual.run_epoch, 1);
    assert_eq!(actual.owner_epoch, fixture.owner.epoch);
}

#[test]
fn startup_busy_agent_api_expiration_fails_closed_without_wait_renewal() {
    let mut fixture = Fixture::new();
    let locker = fixture.take_exclusive();
    let started = Instant::now();
    let error = AgentApi::open(&fixture.path, &fixture.identity_path).unwrap_err();
    let elapsed = started.elapsed();
    assert!(elapsed >= AGENT_STARTUP_BUSY_TIMEOUT.saturating_sub(Duration::from_millis(5)));
    assert!(elapsed < Duration::from_secs(1), "startup used the default five-second SQLite timeout");
    assert!(startup_sqlite_busy(&error) || error.to_string().contains("startup lock window expired"));
    locker.conn.execute_batch("ROLLBACK").unwrap();
    drop(locker);
    assert!(AgentApi::open(&fixture.path, &fixture.identity_path).is_ok());
}

#[test]
fn startup_busy_released_marker_still_refuses_unguarded_fresh_access() {
    let mut fixture = Fixture::new();
    fixture.add_real_managed_marker();
    let locker = fixture.take_exclusive();
    let release = release_after(locker, Duration::from_millis(10), false);
    let result = AgentApi::open(&fixture.path, &fixture.identity_path);
    release.join().unwrap();
    let error = result.unwrap_err();
    assert!(error.to_string().contains("fresh marker refuses unguarded agent access"), "{error:#}");
    let journal = Journal::open(&fixture.path).unwrap();
    assert!(journal.managed_project(&fixture.owner.project_id).unwrap().is_some());
}

#[test]
fn startup_busy_original_run_epoch_is_rechecked_after_release() {
    let mut fixture = Fixture::new();
    let locker = fixture.take_exclusive();
    // An actual concurrent database transaction advances this fixture's run
    // while the startup reader is excluded; the old identity is never rebound.
    locker.conn.execute("UPDATE runs SET epoch=epoch+1 WHERE project_id=?1 AND run_id='run'", [&fixture.owner.project_id]).unwrap();
    let release = release_after(locker, Duration::from_millis(10), true);
    let result = AgentApi::open(&fixture.path, &fixture.identity_path);
    release.join().unwrap();
    let error = result.unwrap_err();
    assert!(error.to_string().contains("stale run epoch"), "{error:#}");
}

#[test]
fn startup_busy_same_deadline_bounds_later_sqlite_waits() {
    let mut fixture = Fixture::new();
    let journal = fixture.journal.take().unwrap();
    journal.conn.execute_batch("CREATE TABLE startup_probe(value INTEGER)").unwrap();
    drop(journal);
    let started = Instant::now();
    let deadline = started + AGENT_STARTUP_BUSY_TIMEOUT;
    let startup = Journal::open_agent_startup_before(&fixture.path, deadline).unwrap();
    let other = Connection::open(&fixture.path).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    // Model time already spent in the refusal/identity stage of this SAME
    // deadline, then exercise the real connection's installed SQLite callback.
    std::thread::sleep(Duration::from_millis(60));
    let error = startup.conn.execute("INSERT INTO startup_probe VALUES(1)", []).unwrap_err();
    assert!(startup_sqlite_busy(&error.into()));
    assert!(Instant::now() >= deadline);
    assert!(started.elapsed() < AGENT_STARTUP_BUSY_TIMEOUT + Duration::from_millis(50),
        "a later statement renewed the original startup wait");
    assert!(startup.finish_agent_startup_before(deadline, AGENT_STARTUP_BUSY_TIMEOUT).is_err());
    other.execute_batch("ROLLBACK").unwrap();
    let count: i64 = startup.conn.query_row("SELECT count(*) FROM startup_probe", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 0, "busy statements must not create a replayed mutation");
}

#[test]
fn startup_busy_success_restores_ordinary_policy_after_original_deadline() {
    let mut fixture = Fixture::new();
    let journal = fixture.journal.take().unwrap();
    journal.conn.execute_batch("CREATE TABLE startup_probe(value INTEGER)").unwrap();
    drop(journal);
    let deadline = Instant::now() + AGENT_STARTUP_BUSY_TIMEOUT;
    let startup = Journal::open_agent_startup_before(&fixture.path, deadline).unwrap();
    startup.finish_agent_startup_before(deadline, AGENT_STARTUP_BUSY_TIMEOUT).unwrap();
    if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        std::thread::sleep(remaining + Duration::from_millis(2));
    }
    let other = Connection::open(&fixture.path).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        other.execute_batch("ROLLBACK").unwrap();
    });
    let result = startup.conn.execute("INSERT INTO startup_probe VALUES(1)", []);
    release.join().unwrap();
    assert_eq!(result.unwrap(), 1, "the old expired startup callback leaked into later IO");
}

#[test]
fn startup_busy_does_not_retry_non_lock_errors_or_open_fresh_identifiers() {
    let mut calls = 0;
    let error = retry_startup_read::<()>(Some(Instant::now() + AGENT_STARTUP_BUSY_TIMEOUT), || {
        calls += 1;
        anyhow::bail!("unsupported managed writer schema")
    }).unwrap_err();
    assert_eq!(calls, 1);
    assert_eq!(error.to_string(), "unsupported managed writer schema");
    let directory = tempfile::tempdir().unwrap();
    for project in [uuid::Uuid::new_v4().to_string(), "fresh-v4:reserved".to_string()] {
        let missing = directory.path().join("must-not-be-created.sqlite");
        assert!(Journal::retain_agent_startup_guard_before(&missing, &project, Instant::now() + AGENT_STARTUP_BUSY_TIMEOUT).is_err());
        assert!(!missing.exists());
    }
}
