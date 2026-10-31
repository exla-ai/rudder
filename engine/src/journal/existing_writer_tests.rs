use super::*;
use std::{os::unix::fs::{symlink, PermissionsExt}, time::Instant};

fn fixture() -> (tempfile::TempDir, Journal, HeldJournalFile) {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().canonicalize().unwrap().join("journal.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    journal.create_project("p", "owner", "base", 0).unwrap();
    let held = journal.hold_database_file().unwrap();
    (directory, journal, held)
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    user_version: u32,
    schema_version: u32,
    data_version: u32,
    mode: String,
    schema: Vec<(String, String, Option<String>)>,
    database: (u64, u64, u64, u32),
    wal: (u64, u64, u64, u32),
}
fn snapshot(journal: &Journal, held: &HeldJournalFile) -> Snapshot {
    let metadata = |path: &Path| {
        let value = fs::symlink_metadata(path).unwrap();
        (value.dev(), value.ino(), value.len(), value.mode())
    };
    Snapshot {
        user_version: journal.conn.pragma_query_value(None, "user_version", |row|row.get(0)).unwrap(),
        schema_version: journal.conn.pragma_query_value(None, "schema_version", |row|row.get(0)).unwrap(),
        data_version: journal.conn.pragma_query_value(None, "data_version", |row|row.get(0)).unwrap(),
        mode: journal.conn.pragma_query_value(None, "journal_mode", |row|row.get(0)).unwrap(),
        schema: journal.conn.prepare("SELECT type,name,sql FROM sqlite_master ORDER BY type,name").unwrap()
            .query_map([], |row|Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap(),
        database: metadata(&held.database.path),
        wal: metadata(&held.wal.path),
    }
}

#[test]
fn existing_writer_is_writable_with_local_full_and_foreign_keys_without_initialization() {
    let (_directory, journal, held) = fixture();
    // These are deliberately different on the original connection. Opening a
    // new writer must enforce its own settings without changing this owner.
    journal.conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=OFF;").unwrap();
    let before = snapshot(&journal, &held);
    let mut writer = held.open_existing_writer(Duration::from_millis(40)).unwrap();
    assert!(!writer.conn.is_readonly(rusqlite::MAIN_DB).unwrap());
    assert!(writer.conn.is_autocommit());
    assert_eq!(writer.conn.pragma_query_value(None, "synchronous", |row|row.get::<_, i64>(0)).unwrap(), 2);
    assert!(writer.conn.pragma_query_value(None, "foreign_keys", |row|row.get::<_, bool>(0)).unwrap());
    assert!(!writer.conn.pragma_query_value(None, "query_only", |row|row.get::<_, bool>(0)).unwrap());
    assert_eq!(writer.conn.pragma_query_value(None, "busy_timeout", |row|row.get::<_, i64>(0)).unwrap(), 40);
    assert_eq!(journal.conn.pragma_query_value(None, "synchronous", |row|row.get::<_, i64>(0)).unwrap(), 1);
    assert!(!journal.conn.pragma_query_value(None, "foreign_keys", |row|row.get::<_, bool>(0)).unwrap());
    assert_eq!(snapshot(&journal, &held), before, "opening must not write schema, database or WAL");

    let invalid = writer.conn.execute("INSERT INTO runs(project_id,run_id,epoch) VALUES('missing','r',1)", []);
    assert!(matches!(invalid, Err(rusqlite::Error::SqliteFailure(error, _))
        if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY));
    writer.create_project("writer", "child-owner", "base", 1).unwrap();
    assert!(journal.project_state("writer").unwrap().owner_active, "a real explicit write must reach the held database");
    assert!(journal.project_state("p").unwrap().owner_active);
    held.check_current().unwrap();
}

#[test]
fn writer_supports_existing_four_and_five_but_refuses_other_versions_without_migration() {
    let (_directory, journal, held) = fixture();
    for version in [4u32, 5, 0, 3, 6] {
        journal.conn.pragma_update(None, "user_version", version).unwrap();
        let before = snapshot(&journal, &held);
        let writer = held.open_existing_writer(Duration::ZERO);
        if matches!(version, 4 | 5) { drop(writer.unwrap()); }
        else { assert!(writer.is_err(), "unsupported schema {version} must not be initialized"); }
        assert_eq!(snapshot(&journal, &held), before);
    }
}

#[test]
fn writer_timeout_is_capped_and_real_writer_contention_remains_bounded() {
    let (_directory, journal, held) = fixture();
    assert!(held.open_existing_writer(Duration::from_millis(251)).is_err());
    let mut writer = held.open_existing_writer(Duration::from_millis(40)).unwrap();
    journal.conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let started = Instant::now();
    let blocked = writer.create_project("blocked", "owner", "base", 1);
    assert!(matches!(blocked, Err(error) if error.downcast_ref::<rusqlite::Error>().is_some_and(|error|
        matches!(error, rusqlite::Error::SqliteFailure(error, _) if error.code == rusqlite::ErrorCode::DatabaseBusy))));
    assert!(started.elapsed() < Duration::from_secs(2), "writer ignored the bounded busy policy");
    journal.conn.execute_batch("ROLLBACK;").unwrap();
    writer.create_project("after-busy", "owner", "base", 2).unwrap();
    assert!(journal.project_state("after-busy").unwrap().owner_active);
}

fn probe_other_process_writer(path: &Path, busy: bool) {
    struct Probe(std::process::Child);
    impl Drop for Probe {
        fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
    }
    // Reuse the existing independent SQLite lock probe. Never inspect SQLite
    // inodes by raw open/close: that could invalidate this process's locks.
    let mut child = Probe(std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "journal::readonly::tests::writer_lock_probe_child", "--ignored", "--nocapture"])
        .env_clear().env("RUDDER_TEST_JOURNAL_WRITER_DB", path)
        .env("RUDDER_TEST_JOURNAL_WRITER_BUSY", if busy { "yes" } else { "no" })
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() { break status; }
        assert!(Instant::now() < deadline, "fresh-process SQLite writer probe did not settle");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success(), "fresh-process writer exclusion was wrong for busy={busy}: {status}");
}

#[test]
fn opening_and_dropping_writers_preserves_other_process_wal_writer_exclusion() {
    let (_directory, journal, held) = fixture();
    journal.conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    probe_other_process_writer(&held.database.path, true);
    for _ in 0..3 {
        let writer = held.open_existing_writer(Duration::ZERO).unwrap();
        probe_other_process_writer(&held.database.path, true);
        drop(writer);
        probe_other_process_writer(&held.database.path, true);
    }
    journal.conn.execute_batch("ROLLBACK;").unwrap();
    probe_other_process_writer(&held.database.path, false);
}

#[test]
fn writer_refuses_missing_replaced_or_unsafe_files_without_creating_or_repairing() {
    let (directory, _journal, held) = fixture();
    let retained = directory.path().join("retained.sqlite");
    fs::rename(&held.database.path, &retained).unwrap();
    assert!(held.open_existing_writer(Duration::ZERO).is_err());
    assert!(!held.database.path.exists(), "must not CREATE a missing database");
    symlink(&retained, &held.database.path).unwrap();
    assert!(held.open_existing_writer(Duration::ZERO).is_err());
    fs::remove_file(&held.database.path).unwrap();
    fs::rename(&retained, &held.database.path).unwrap();

    fs::set_permissions(&held.database.path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(held.open_existing_writer(Duration::ZERO).is_err());
    assert_eq!(fs::symlink_metadata(&held.database.path).unwrap().mode() & 0o777, 0o644, "must not chmod unsafe state");
    fs::set_permissions(&held.database.path, fs::Permissions::from_mode(0o600)).unwrap();
    let alias = directory.path().join("alias.sqlite");
    fs::hard_link(&held.database.path, &alias).unwrap();
    assert!(held.open_existing_writer(Duration::ZERO).is_err());
    fs::remove_file(&alias).unwrap();

    // Create the replacement through SQLite itself, not a raw DB descriptor.
    let replacement = directory.path().join("replacement.sqlite");
    let other = Connection::open(&replacement).unwrap();
    other.execute_batch("PRAGMA user_version=4;").unwrap();
    drop(other);
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&held.database.path, &retained).unwrap();
    fs::rename(&replacement, &held.database.path).unwrap();
    assert!(held.open_existing_writer(Duration::ZERO).is_err(), "must reject a different actual SQLite inode");
    fs::rename(&held.database.path, &replacement).unwrap();
    fs::rename(&retained, &held.database.path).unwrap();
    held.check_current().unwrap();

    for auxiliary in [&held.wal.path, &held.shm.path] {
        let retained = directory.path().join("retained-auxiliary");
        fs::rename(auxiliary, &retained).unwrap();
        assert!(held.open_existing_writer(Duration::ZERO).is_err());
        assert!(!auxiliary.exists(), "must not recreate a missing WAL/SHM inode");
        fs::rename(retained, auxiliary).unwrap();
    }
}
