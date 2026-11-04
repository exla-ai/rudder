use super::*;
use std::{
    os::unix::fs::{symlink, PermissionsExt},
    time::Instant,
};

fn fixture() -> (tempfile::TempDir, Journal, HeldJournalFile) {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory
        .path()
        .canonicalize()
        .unwrap()
        .join("journal.sqlite");
    let mut journal = Journal::open(&path).unwrap();
    journal.create_project("p", "owner", "base", 0).unwrap();
    let held = journal.hold_database_file().unwrap();
    (directory, journal, held)
}

#[test]
fn main_database_open_is_readonly_without_schema_or_mode_mutation() {
    let (_directory, journal, held) = fixture();
    let database = fs::read(&held.database.path).unwrap();
    let wal = fs::read(&held.wal.path).unwrap();
    let read = held.open_read_only(Duration::from_millis(50)).unwrap();
    assert!(read.journal.conn.is_readonly(rusqlite::MAIN_DB).unwrap());
    assert_eq!(
        read.journal
            .conn
            .pragma_query_value(None, "query_only", |r| r.get::<_, bool>(0))
            .unwrap(),
        true
    );
    assert_eq!(
        read.journal
            .conn
            .pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))
            .unwrap(),
        true
    );
    assert!(read
        .journal
        .conn
        .execute("CREATE TABLE unexpected(value INTEGER)", [])
        .is_err());
    // READ_ONLY is a separate physical fence even if a private test disables
    // query_only. No caller receives access to this connection.
    read.journal
        .conn
        .execute_batch("PRAGMA query_only=OFF;")
        .unwrap();
    assert!(read
        .journal
        .conn
        .execute("UPDATE projects SET owner_active=0", [])
        .is_err());
    drop(read);
    assert_eq!(fs::read(&held.database.path).unwrap(), database);
    assert_eq!(fs::read(&held.wal.path).unwrap(), wal);
    held.check_current().unwrap();
    assert!(journal.project_state("p").unwrap().owner_active);
}

#[test]
fn schema_four_and_five_open_without_initialization() {
    let (_directory, mut journal, held) = fixture();
    for version in [4u32, 5] {
        journal
            .conn
            .pragma_update(None, "user_version", version)
            .unwrap();
        let before = fs::read(&held.wal.path).unwrap();
        let read = held.open_read_only(Duration::ZERO).unwrap();
        assert_eq!(
            read.journal
                .conn
                .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            version
        );
        drop(read);
        assert_eq!(fs::read(&held.wal.path).unwrap(), before);
    }
    for mode in ["PASSIVE", "TRUNCATE"] {
        let (busy, _, _): (i64, i64, i64) = journal
            .conn
            .query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(busy, 0, "the keeper must not hold a read transaction");
        held.check_current().unwrap();
        let project = format!("after-{mode}");
        journal
            .create_project(&project, "owner", "base", 1)
            .unwrap();
        let fresh = held.open_read_only(Duration::from_millis(50)).unwrap();
        assert!(
            fresh.journal.project_state(&project).unwrap().owner_active,
            "fresh request must observe the actual write after checkpoint"
        );
    }
}

#[test]
fn old_empty_and_future_schema_are_refused_without_migration() {
    let (_directory, journal, held) = fixture();
    for version in [0u32, 3, 6] {
        journal
            .conn
            .pragma_update(None, "user_version", version)
            .unwrap();
        let before = fs::read(&held.wal.path).unwrap();
        assert!(held.open_read_only(Duration::from_millis(50)).is_err());
        assert_eq!(
            journal
                .conn
                .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            version
        );
        assert_eq!(fs::read(&held.wal.path).unwrap(), before);
    }
}

#[test]
fn readonly_busy_timeout_is_capped_and_actual_exclusive_contention_is_bounded() {
    let (_directory, journal, held) = fixture();
    assert!(held.open_read_only(Duration::from_millis(251)).is_err());
    let path = held.database.path.clone();
    // An idle WAL keeper intentionally retains a SQLite connection. Drop it
    // before asking this sole connection to take whole-database exclusivity.
    drop(held);
    journal
        .conn
        .execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;")
        .unwrap();
    let started = Instant::now();
    let result = open_existing_connection(&path, Duration::from_millis(40));
    assert!(
        result.is_err(),
        "actual exclusive SQLite owner must exclude this reader"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "reader ignored bounded busy admission"
    );
    journal.conn.execute_batch("ROLLBACK;").unwrap();
}

fn probe_writer_in_fresh_process(path: &Path, busy: bool) {
    struct Probe(std::process::Child);
    impl Drop for Probe {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Probe(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "journal::readonly::tests::writer_lock_probe_child",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("RUDDER_TEST_JOURNAL_WRITER_DB", path)
            .env(
                "RUDDER_TEST_JOURNAL_WRITER_BUSY",
                if busy { "yes" } else { "no" },
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "fresh SQLite writer probe did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(
        status.success(),
        "fresh SQLite writer observed wrong busy={busy}: {status}"
    );
}

#[test]
#[ignore = "bounded self-exec SQLite writer probe invoked by actual lock test"]
fn writer_lock_probe_child() {
    let path = std::env::var_os("RUDDER_TEST_JOURNAL_WRITER_DB")
        .expect("explicit fixture database required");
    let conn = Connection::open_with_flags(
        PathBuf::from(path),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    conn.busy_timeout(Duration::ZERO).unwrap();
    let result = conn.execute_batch("BEGIN IMMEDIATE;");
    match std::env::var("RUDDER_TEST_JOURNAL_WRITER_BUSY")
        .unwrap()
        .as_str()
    {
        "yes" => assert!(
            matches!(result,Err(rusqlite::Error::SqliteFailure(error,_))
            if error.code==rusqlite::ErrorCode::DatabaseBusy),
            "actual other-process WAL writer lock missing"
        ),
        "no" => {
            result.unwrap();
            conn.execute_batch("ROLLBACK;").unwrap();
        }
        _ => panic!("explicit writer-lock expectation required"),
    }
}

#[test]
fn dropping_sqlite_keeper_preserves_actual_other_process_writer_exclusion() {
    let (_directory, journal, held) = fixture();
    let path = held.database.path.clone();
    journal.conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    probe_writer_in_fresh_process(&path, true);
    drop(held);
    // An ordinary std::File close on the SHM inode would release the process's
    // fcntl writer lock here, despite this live SQLite transaction's bookkeeping.
    probe_writer_in_fresh_process(&path, true);
    journal.conn.execute_batch("ROLLBACK;").unwrap();
    probe_writer_in_fresh_process(&path, false);
}

#[test]
fn moved_actual_sqlite_file_cannot_be_reanchored_from_its_old_filename() {
    let (directory, journal, _held) = fixture();
    let named = journal.database_path().unwrap();
    let retained = directory.path().join("retained.sqlite");
    fs::rename(named, &retained).unwrap();
    fs::copy(&retained, named).unwrap();
    assert!(
        journal.hold_database_file().is_err(),
        "SQLITE_FCNTL_HAS_MOVED must bind the original actual connection"
    );
    fs::remove_file(named).unwrap();
    fs::rename(retained, named).unwrap();
}

#[test]
fn missing_database_is_not_created_by_readonly_open() {
    let (directory, _journal, held) = fixture();
    let retained = directory.path().join("retained.sqlite");
    fs::rename(&held.database.path, &retained).unwrap();
    assert!(held.open_read_only(Duration::ZERO).is_err());
    assert!(!held.database.path.exists());
    fs::rename(retained, &held.database.path).unwrap();
}

#[test]
fn readonly_anchor_refuses_mode_hardlink_symlink_and_missing_auxiliary_files() {
    let (directory, _journal, held) = fixture();
    fs::set_permissions(&held.database.path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(held.open_read_only(Duration::ZERO).is_err());
    assert_eq!(
        fs::metadata(&held.database.path).unwrap().mode() & 0o777,
        0o644,
        "read-only opening must never chmod an unsafe file"
    );
    fs::set_permissions(&held.database.path, fs::Permissions::from_mode(0o600)).unwrap();
    let alias = directory.path().join("alias.sqlite");
    fs::hard_link(&held.database.path, &alias).unwrap();
    assert!(held.open_read_only(Duration::ZERO).is_err());
    fs::remove_file(&alias).unwrap();
    fs::rename(&held.database.path, &alias).unwrap();
    symlink(&alias, &held.database.path).unwrap();
    assert!(held.open_read_only(Duration::ZERO).is_err());
    fs::remove_file(&held.database.path).unwrap();
    fs::rename(&alias, &held.database.path).unwrap();
    let missing = directory.path().join("retained-shm");
    fs::rename(&held.shm.path, &missing).unwrap();
    assert!(held.open_read_only(Duration::ZERO).is_err());
    assert!(
        !held.shm.path.exists(),
        "must not recreate a missing SHM file"
    );
    fs::rename(missing, &held.shm.path).unwrap();
}
