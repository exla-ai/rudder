//! Existing Journal connections without initialization. Held file identity is
//! local custody only; it grants no project/run/source permission.
use super::*;
use std::{
    fs::{self, File, Metadata, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::Mutex,
};

pub const READ_ONLY_MAX_BUSY_TIMEOUT: Duration = Duration::from_millis(250);

struct JournalFileIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl JournalFileIdentity {
    fn capture(path: PathBuf) -> Result<Self> {
        // Never independently open/close SQLite inodes: POSIX close() would
        // release the process's other live SQLite fcntl locks on that inode.
        let metadata = fs::symlink_metadata(&path)?;
        check_private_file(&metadata)?;
        let result = Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        result.check_current()?;
        Ok(result)
    }

    fn check_current(&self) -> Result<()> {
        let named = fs::symlink_metadata(&self.path)?;
        check_private_file(&named)?;
        ensure!(
            named.dev() == self.device && named.ino() == self.inode,
            "held Journal file was replaced"
        );
        Ok(())
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn check_private_file(meta: &Metadata) -> Result<()> {
    ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.nlink() == 1
            && meta.mode() & 0o7777 == 0o600,
        "Journal file must remain a single-link UID-owned0600 regular file"
    );
    Ok(())
}

/// A real held file/parent derived from an already-open Journal connection.
/// Never deserialize this value or substitute a caller-selected path as proof.
/// Opening/checking it performs filesystem IO and must run off App.
pub struct HeldJournalFile {
    // Actual database descriptors stay under SQLite VFS custody, including its
    // process-wide lock/deferred-close bookkeeping. No read transaction is kept.
    keeper: Mutex<Connection>,
    database: JournalFileIdentity,
    wal: JournalFileIdentity,
    shm: JournalFileIdentity,
    parent: File,
    parent_path: PathBuf,
    parent_device: u64,
    parent_inode: u64,
}

impl Journal {
    /// Off-App. Bind local file custody to this actual existing SQLite
    /// connection, not just its filename. Unknown VFS/moved files fail closed.
    pub fn hold_database_file(&self) -> Result<HeldJournalFile> {
        check_sqlite_file_current(&self.conn)?;
        let path = self.database_path()?.to_path_buf();
        ensure!(
            path.is_absolute() && path.canonicalize()? == path,
            "Journal file path must be canonical and absolute"
        );
        let parent_path = path
            .parent()
            .context("Journal has no parent")?
            .to_path_buf();
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&parent_path)?;
        let meta = parent.metadata()?;
        check_private_parent(&meta)?;
        let database = JournalFileIdentity::capture(path)?;
        let wal = JournalFileIdentity::capture(with_suffix(&database.path, "-wal"))?;
        let shm = JournalFileIdentity::capture(with_suffix(&database.path, "-shm"))?;
        let keeper = open_existing_connection(&database.path, READ_ONLY_MAX_BUSY_TIMEOUT)?;
        let result = HeldJournalFile {
            keeper: Mutex::new(keeper),
            database,
            wal,
            shm,
            parent,
            parent_path,
            parent_device: meta.dev(),
            parent_inode: meta.ino(),
        };
        check_sqlite_file_current(&self.conn)?;
        result.check_current()?;
        Ok(result)
    }
}

fn check_private_parent(meta: &Metadata) -> Result<()> {
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o7777 == 0o700,
        "Journal parent must remain UID-owned0700"
    );
    Ok(())
}

impl HeldJournalFile {
    pub fn check_current(&self) -> Result<()> {
        let held = self.parent.metadata()?;
        let named = fs::symlink_metadata(&self.parent_path)?;
        check_private_parent(&held)?;
        check_private_parent(&named)?;
        ensure!(
            held.dev() == self.parent_device
                && held.ino() == self.parent_inode
                && named.dev() == self.parent_device
                && named.ino() == self.parent_inode,
            "Journal parent was replaced"
        );
        ensure!(
            self.database.path.canonicalize()? == self.database.path,
            "Journal path gained a symbolic link"
        );
        self.database.check_current()?;
        self.wal.check_current()?;
        self.shm.check_current()?;
        // Only this bounded SQLite file-control check runs under the keeper
        // mutex. No authorization query or caller callback can retain it.
        let keeper = self
            .keeper
            .lock()
            .map_err(|_| anyhow::anyhow!("Journal keeper is poisoned"))?;
        check_sqlite_file_current(&keeper)
    }

    /// No CREATE, migration, chmod, writer PRAGMAs or immutable live snapshot.
    /// SQLite's normal live WAL reader may coordinate the existing SHM file;
    /// its three retained files must already exist before and after opening.
    pub fn open_read_only(&self, busy_timeout: Duration) -> Result<ReadOnlyJournal<'_>> {
        self.check_current()?;
        let conn = open_existing_connection(&self.database.path, busy_timeout)?;
        self.check_current()?;
        Ok(ReadOnlyJournal {
            journal: Journal { conn },
            held: self,
        })
    }

    /// Off-App privileged connection for this actual held database. No CREATE,
    /// schema initialization/migration, chmod or journal-mode change is allowed.
    /// FULL and foreign_keys are enforced only on this new connection. The
    /// caller must separately retain/check its actual owner/run authority.
    pub fn open_existing_writer(&self, busy_timeout: Duration) -> Result<Journal> {
        ensure!(busy_timeout <= READ_ONLY_MAX_BUSY_TIMEOUT,
            "existing writer Journal busy timeout exceeds250ms");
        ensure!(rusqlite::version_number() >= 3_051_003,
            "SQLite >= 3.51.3 is required for WAL safety");
        self.check_current()?;
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
        // SQLite owns all database/WAL/SHM descriptors and deferred closes.
        // Never independently open/close their inodes to inspect custody.
        let conn = Connection::open_with_flags(&self.database.path, flags)?;
        conn.busy_timeout(busy_timeout)?;
        check_sqlite_file_current(&conn)?;
        self.check_current()?;
        ensure!(!conn.is_readonly(rusqlite::MAIN_DB)? && conn.is_autocommit(),
            "existing Journal did not open as an idle writable connection");
        validate_existing_schema(&conn)?;
        // Both settings are connection-local. In particular, do not issue
        // PRAGMA journal_mode=WAL, which could mutate an unexpected database.
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        let synchronous: i64 = conn.pragma_query_value(None, "synchronous", |row|row.get(0))?;
        let foreign_keys: bool = conn.pragma_query_value(None, "foreign_keys", |row|row.get(0))?;
        let query_only: bool = conn.pragma_query_value(None, "query_only", |row|row.get(0))?;
        ensure!(synchronous == 2 && foreign_keys && !query_only && conn.is_autocommit(),
            "existing Journal writer requires FULL synchronous and foreign keys");
        validate_existing_schema(&conn)?;
        check_sqlite_file_current(&conn)?;
        self.check_current()?;
        Ok(Journal { conn })
    }
}

fn open_existing_connection(path: &Path, busy_timeout: Duration) -> Result<Connection> {
    ensure!(
        busy_timeout <= READ_ONLY_MAX_BUSY_TIMEOUT,
        "read-only Journal busy timeout exceeds250ms"
    );
    ensure!(
        rusqlite::version_number() >= 3_051_003,
        "SQLite >= 3.51.3 is required for WAL safety"
    );
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
        | rusqlite::OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(busy_timeout)?;
    ensure!(
        conn.is_readonly(rusqlite::MAIN_DB)?,
        "existing Journal did not open read-only"
    );
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA foreign_keys=ON;")?;
    validate_existing_schema(&conn)?;
    check_sqlite_file_current(&conn)?;
    Ok(conn)
}

fn validate_existing_schema(conn: &Connection) -> Result<()> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(
        matches!(version, 4 | 5),
        "existing Journal schema must be4or5"
    );
    let mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    ensure!(mode == "wal", "existing Journal must already use WAL");
    Ok(())
}

fn check_sqlite_file_current(conn: &Connection) -> Result<()> {
    let mut moved: libc::c_int = -1;
    // SQLite's supported VFS control checks the real opened database file,
    // rather than assuming that sqlite3_db_filename still identifies its inode.
    let code = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
            (&mut moved as *mut libc::c_int).cast(),
        )
    };
    ensure!(
        code == rusqlite::ffi::SQLITE_OK && moved == 0,
        "actual SQLite Journal file moved or lacks a supported custody check"
    );
    Ok(())
}

/// A bounded, existing, read-only connection. No mutating Journal API is exposed.
pub struct ReadOnlyJournal<'a> {
    journal: Journal,
    held: &'a HeldJournalFile,
}

impl ReadOnlyJournal<'_> {
    /// Uses this actual held existing Journal and one fresh read transaction.
    /// No caller-supplied path, CREATE, migration, issuance or renewal is used.
    #[cfg(target_os = "linux")]
    pub fn authorize_host_connect(
        &self,
        token: &str,
        binding: &crate::execution::network::connected::ConnectBinding,
        expected_expiry_ms: i64,
        now_ms: i64,
    ) -> Result<HostConnectAuthorization> {
        self.held.check_current()?;
        check_sqlite_file_current(&self.journal.conn)?;
        let tx = self.journal.conn.unchecked_transaction()?;
        validate_existing_schema(&tx)?;
        let result = Journal::authorize_host_connect_transaction(
            &tx, token, binding, expected_expiry_ms, now_ms);
        check_sqlite_file_current(&tx)?;
        self.held.check_current()?;
        tx.commit()?;
        result
    }

    pub fn authorize_host_source(
        &self,
        token: &str,
        binding: &crate::vfs::partial_rpc::PartialBinding,
        now_ms: i64,
    ) -> Result<HostSourceAuthorization> {
        self.held.check_current()?;
        check_sqlite_file_current(&self.journal.conn)?;
        let tx = self.journal.conn.unchecked_transaction()?;
        validate_existing_schema(&tx)?;
        let result = Journal::authorize_host_source_transaction(&tx, token, binding, now_ms);
        check_sqlite_file_current(&tx)?;
        self.held.check_current()?;
        tx.commit()?;
        result
    }
}

#[cfg(test)]
#[path = "readonly_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "existing_writer_tests.rs"]
mod writer_tests;
