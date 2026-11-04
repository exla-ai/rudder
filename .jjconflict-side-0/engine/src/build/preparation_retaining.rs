//! Additive preparation of already-built immutable content. This is local
//! preparation/index custody, never project, process, lease or task authority.
//! The original carrier must remain outside catch/unwind and on every error.
use super::*;
use crate::{execution::finalization_budget::{FinalizationBudget, FinalizationConstraint},
    store::{preparation::PreparationStore, ImageImportFence}};
use std::{fs::Metadata,
    os::unix::fs::{MetadataExt, OpenOptionsExt}, sync::{Arc, MutexGuard, TryLockError, atomic::{AtomicBool, Ordering}}};

const MAX_ENCODING: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedPreparationStatus {
    Captured, WaitingKey, WaitingIndex, Inspecting, Staging, Publishing,
    Inserting, Committing, Acknowledged, Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SqlOutcome { Unopened, OpenAttempted, Opened, BeginAttempted, Begun, InsertAttempted, Inserted, CommitAttempted, Committed }

/// Only one original successful execution/retained-index check issues this
/// owner. A PreparedEnvironment's Serde DATA cannot recreate it. The enclosing
/// RetainedPreparation keeps the real index, source anchors and original IO.
pub struct PreparationAcknowledgment { prepared: PreparedEnvironment, reused: bool }
impl PreparationAcknowledgment {
    pub fn prepared(&self) -> &PreparedEnvironment { &self.prepared }
    pub fn reused(&self) -> bool { self.reused }
}

struct OriginalClock { original: FinalizationBudget, cancelled: Arc<AtomicBool> }
impl ImageImportFence for OriginalClock {
    fn remaining(&self) -> Result<Duration> {
        if self.cancelled.load(Ordering::Acquire) { self.original.cancel(); }
        self.original.remaining()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Inode { dev: u64, ino: u64, mode: u32, uid: u32, gid: u32 }
impl Inode {
    fn of(m: &Metadata) -> Self { Self { dev: m.dev(), ino: m.ino(), mode: m.mode(), uid: m.uid(), gid: m.gid() } }
}
struct IndexWitness { root: Option<File>, locks: Option<File>,
    root_id: Option<Inode>, locks_id: Option<Inode>, database_id: Option<Inode> }
impl IndexWitness {
    fn staged() -> Self { Self { root: None, locks: None, root_id: None, locks_id: None, database_id: None } }
    fn check(&self, index: &BuildIndex) -> Result<()> {
        for (file, id, path) in [(&self.root, &self.root_id, index.root.clone()),
            (&self.locks, &self.locks_id, index.locks.clone())] {
            if let (Some(file), Some(id)) = (file, id) {
                let held = file.metadata()?; let named = fs::symlink_metadata(path)?;
                ensure!(Inode::of(&held) == *id && Inode::of(&named) == *id && !named.file_type().is_symlink(),
                    "original preparation index inode changed");
            }
        }
        if let Some(id) = &self.database_id {
            let named = fs::symlink_metadata(index.root.join("index.sqlite"))?;
            ensure!(Inode::of(&named) == *id && named.is_file() && named.nlink() == 1,
                "original preparation database binding changed");
        }
        Ok(())
    }
}

/// Non-Serde, no Clone, no Drop IO, no resume/readback constructor. Failure or
/// panic retains actual GC/key FDs, SQLite transaction, staging/pin/SQL intents,
/// pending metadata/file IO and known outcome. Expiry never releases them.
#[must_use = "keep the actual original preparation carrier on every outcome"]
pub struct RetainedPreparation {
    index: Arc<BuildIndex>, spec: PreparationSpec, layout: PreparedContentLayout,
    directory: ObjectRef, anchors: Vec<ObjectRef>, original: FinalizationBudget,
    clock: Arc<OriginalClock>, status: RetainedPreparationStatus,
    witness: IndexWitness, key_file: Option<File>, key_locked: bool,
    connection: Option<Connection>, sql: SqlOutcome, store: Option<PreparationStore>,
    expected: Option<PreparedEnvironment>, bytes: Option<[Vec<u8>; 3]>,
    object_attempts: usize, pin_attempted: bool, pin_generation: Option<u64>,
    ack: Option<PreparationAcknowledgment>, failure: Option<anyhow::Error>,
    #[cfg(test)] fault: Option<TestPoint>,
}
impl RetainedPreparation {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn stage(index: Arc<BuildIndex>, spec: PreparationSpec, layout: PreparedContentLayout,
        directory: ObjectRef, anchors: Vec<ObjectRef>, original: FinalizationBudget, cancelled: Arc<AtomicBool>) -> Self {
        let clock = Arc::new(OriginalClock { original: original.clone(), cancelled: cancelled.clone() });
        Self { index, spec, layout, directory, anchors, original, clock,
            status: RetainedPreparationStatus::Captured, witness: IndexWitness::staged(),
            key_file: None, key_locked: false, connection: None, sql: SqlOutcome::Unopened, store: None,
            expected: None, bytes: None, object_attempts: 0, pin_attempted: false, pin_generation: None,
            ack: None, failure: None, #[cfg(test)] fault: None }
    }
    pub fn status(&self) -> RetainedPreparationStatus { self.status }
    pub fn original_constraint_data(&self) -> FinalizationConstraint { self.original.constraint() }
    pub fn key_data(&self) -> Option<&ObjectRef> { self.expected.as_ref().map(|p| &p.key) }
    pub fn staging_id_data(&self) -> Option<&str> { self.store.as_ref().and_then(|s| s.lease.as_ref()).map(|s| s.id.as_str()) }
    pub fn pin_attempted_data(&self) -> bool { self.pin_attempted }
    pub fn acknowledgment(&self) -> Result<&PreparationAcknowledgment> {
        ensure!(self.status == RetainedPreparationStatus::Acknowledged, "original preparation ACK absent/unknown");
        self.ack.as_ref().context("original preparation ACK absent")
    }
    pub fn failure(&self) -> Option<&anyhow::Error> { self.failure.as_ref() }
    fn check(&self) -> Result<()> {
        self.clock.remaining()?; self.witness.check(&self.index)?;
        // SAME original BuildIndex connection is the keeper. SQLite alone owns
        // database/WAL/SHM descriptors and their deferred-close bookkeeping.
        // Raw File open/close on index.sqlite would release POSIX locks held by
        // other connections in this process, even on successful carrier disposal.
        {
            let keeper = self.original_keeper()?;
            sqlite_binding_current(&keeper, &self.index.root.join("index.sqlite"))?;
        }
        if let Some(connection) = &self.connection {
            sqlite_binding_current(connection, &self.index.root.join("index.sqlite"))?;
        }
        self.clock.remaining()?; Ok(())
    }
    fn original_keeper(&self) -> Result<MutexGuard<'_, Connection>> {
        loop {
            self.clock.remaining()?;
            match self.index.connection.try_lock() {
                Ok(guard) => { self.clock.remaining()?; return Ok(guard); }
                Err(TryLockError::WouldBlock) => {
                    std::thread::sleep(self.clock.remaining()?.min(Duration::from_millis(1)));
                }
                Err(TryLockError::Poisoned(_)) => anyhow::bail!("original preparation index keeper poisoned"),
            }
        }
    }
    pub(super) fn run(&mut self) -> Result<()> {
        ensure!(self.status == RetainedPreparationStatus::Captured, "original preparation cannot retry");
        let result = self.run_inner();
        if let Err(error) = result {
            self.status = RetainedPreparationStatus::Failed;
            if let Some(store) = &mut self.store { store.fail(); }
            self.failure = Some(error);
            return Err(anyhow::anyhow!("original retained preparation failed; keep carrier"));
        }
        Ok(())
    }
    fn encode(&mut self) -> Result<()> {
        self.check()?;
        ensure!(self.spec.installer_commands.is_empty(), "retained pinned preparation does not execute installers");
        ensure!(self.directory == self.spec.base_runtime, "retained preparation requires its already-built base directory");
        ensure!(self.spec.toolchains.len() <= 64 && self.spec.installer_inputs.len() <= 1024
            && self.anchors.len() <= 1024 && self.spec.platform.properties.len() <= 256
            && self.spec.installer_environment.len() <= 256 && self.spec.policy_version.len() <= 4096,
            "preparation input profile exceeds bounds");
        let mut scalar_bytes = 0usize;
        for value in [&self.spec.platform.os, &self.spec.platform.architecture, &self.spec.platform.abi, &self.spec.policy_version] {
            scalar_bound(&mut scalar_bytes, value)?;
        }
        for (key, value) in self.spec.platform.properties.iter().chain(self.spec.installer_environment.iter()) {
            scalar_bound(&mut scalar_bytes, key)?; scalar_bound(&mut scalar_bytes, value)?;
        }
        for key in self.spec.installer_inputs.keys() { scalar_bound(&mut scalar_bytes, key)?; }
        for (name, tool) in &self.spec.toolchains {
            self.check()?;
            ensure!(tool.configuration.len() <= 256 && tool.executable_path.len() <= 4096 && tool.version.len() <= 4096,
                "preparation tool profile exceeds bounds");
            for value in [name, &tool.version, &tool.executable_path] { scalar_bound(&mut scalar_bytes, value)?; }
            for (key, value) in &tool.configuration { scalar_bound(&mut scalar_bytes, key)?; scalar_bound(&mut scalar_bytes, value)?; }
        }
        self.spec.validate()?;
        if self.layout == PreparedContentLayout::PagedChunkedV2 {
            ensure!(self.spec.installer_inputs.values().all(|r| matches!(r.kind, ObjectKind::Blob | ObjectKind::File | ObjectKind::Directory)),
                "ancestry-free preparation requires content-only installer inputs");
        }
        let mut inputs: Vec<_> = self.spec.installer_inputs.values().cloned().collect();
        inputs.push(self.spec.base_runtime.clone());
        for tool in self.spec.toolchains.values() { inputs.extend([tool.closure.clone(), tool.executable.clone()]); }
        inputs.sort(); inputs.dedup();
        let key_bytes = encode_metadata(self.layout.key_schema(), &self.spec, inputs.clone())?;
        let key = ObjectRef::from_bytes(ObjectKind::ActionKey, &key_bytes);
        let provenance = BTreeMap::from([("preparation_key".to_owned(), key.digest.clone()),
            ("policy_version".to_owned(), self.spec.policy_version.clone())]);
        let root_bytes = match self.layout {
            PreparedContentLayout::LegacyV1 => canonical_json(&EnvironmentManifest { format_version: FORMAT_VERSION,
                directory: self.directory.clone(), platform: self.spec.platform.properties(), installer_inputs: inputs, provenance })?,
            PreparedContentLayout::PagedChunkedV2 => canonical_json(&crate::snapshot::EnvironmentV2 { format_version: 2,
                directory: self.directory.clone(), platform: self.spec.platform.properties(), installer_inputs: inputs, provenance })?,
        };
        let root = ObjectRef::from_bytes(ObjectKind::EnvironmentRoot, &root_bytes);
        let receipt_bytes = encode_metadata(self.layout.receipt_schema(), &serde_json::json!({"key":key,"root":root}), vec![key.clone(), root.clone()])?;
        let receipt = ObjectRef::from_bytes(ObjectKind::ActionResult, &receipt_bytes);
        ensure!([key_bytes.len(), root_bytes.len(), receipt_bytes.len()].iter().all(|n| *n <= MAX_ENCODING), "preparation encoding exceeds profile");
        // Metadata references/schema must match before any staging/object write.
        for (kind, bytes) in [(ObjectKind::ActionKey, &key_bytes), (ObjectKind::EnvironmentRoot, &root_bytes), (ObjectKind::ActionResult, &receipt_bytes)] {
            crate::store::metadata_references(kind, bytes)?;
        }
        self.expected = Some(PreparedEnvironment { key, root, receipt }); self.bytes = Some([key_bytes, root_bytes, receipt_bytes]); self.check()
    }
    fn capture_index(&mut self) -> Result<()> {
        self.check()?;
        ensure!(self.index.root.canonicalize()? == self.index.root && self.index.locks.canonicalize()? == self.index.locks,
            "preparation index redirects");
        self.witness.root = Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW).open(&self.index.root)?);
        self.witness.root_id = Some(Inode::of(&self.witness.root.as_ref().unwrap().metadata()?));
        self.check()?;
        self.witness.locks = Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW).open(&self.index.locks)?);
        self.witness.locks_id = Some(Inode::of(&self.witness.locks.as_ref().unwrap().metadata()?));
        self.check()?;
        // Stat only, under the original SQLite keeper and bracketed by VFS
        // HAS_MOVED checks. Never independently open the database inode.
        let database = {
            let keeper = self.original_keeper()?;
            let path = self.index.root.join("index.sqlite");
            sqlite_binding_current(&keeper, &path)?;
            let named = fs::symlink_metadata(&path)?;
            sqlite_binding_current(&keeper, &path)?;
            named
        };
        self.witness.database_id = Some(Inode::of(&database));
        for file in [self.witness.root.as_ref().unwrap(), self.witness.locks.as_ref().unwrap()] {
            let m = file.metadata()?;
            ensure!(m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0, "original preparation index ownership/mode invalid");
        }
        self.check()
    }
    fn key_lock(&mut self) -> Result<()> {
        self.status = RetainedPreparationStatus::WaitingKey;
        let path = self.index.locks.join(format!("{}-{}", self.layout.category(), self.expected.as_ref().unwrap().key.digest));
        self.check()?;
        self.key_file = Some(OpenOptions::new().read(true).write(true).create(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK).open(path)?);
        ensure!(self.key_file.as_ref().unwrap().metadata()?.is_file(), "preparation key lock not regular");
        loop {
            self.check()?;
            match FileExt::try_lock_exclusive(self.key_file.as_ref().unwrap()) {
                Ok(()) => { self.key_locked = true; return self.check(); }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => self.wait()?,
                Err(error) => return Err(error.into()),
            }
        }
    }
    fn wait(&self) -> Result<()> {
        let remaining = self.clock.remaining()?; std::thread::sleep(remaining.min(Duration::from_millis(1))); self.check()
    }
    fn open_sqlite(&mut self) -> Result<()> {
        self.status = RetainedPreparationStatus::WaitingIndex; self.check()?;
        // Dedicated connection: a retained/uncertain transaction never bleeds
        // into the legacy index.connection mutex or an unrelated CLI action.
        self.sql = SqlOutcome::OpenAttempted;
        self.connection = Some(Connection::open_with_flags(self.index.root.join("index.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW | rusqlite::OpenFlags::SQLITE_OPEN_PRIVATE_CACHE)?);
        self.sql = SqlOutcome::Opened; self.check()?;
        let clock = self.clock.clone();
        let connection = self.connection.as_mut().unwrap();
        connection.busy_timeout(Duration::ZERO)?;
        connection.progress_handler(1000, Some(move || clock.remaining().is_err()))?;
        let journal: String = connection.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        let sync: i64 = connection.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
        ensure!(journal.eq_ignore_ascii_case("wal") && sync == 2, "retained preparation requires existing WAL/FULL index; no migration");
        self.check()?;
        loop {
            self.check()?; self.sql = SqlOutcome::BeginAttempted;
            match self.connection.as_ref().unwrap().execute_batch("BEGIN IMMEDIATE") {
                Ok(()) => { self.sql = SqlOutcome::Begun; return self.check(); }
                Err(error) if matches!(error.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)) => {
                    // Known BEGIN contention has not inserted/published anything.
                    // It alone may wait/retry, always under the same original.
                    ensure!(self.connection.as_ref().unwrap().is_autocommit(), "BEGIN contention retained an uncertain transaction");
                    self.wait()?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    fn lookup(&self) -> Result<Option<ObjectRef>> {
        self.check()?; let expected = self.expected.as_ref().unwrap();
        let row: Option<(i64, Option<String>, Option<String>, i64, i64)> = self.connection.as_ref().unwrap().query_row(
            "SELECT format_version,CASE WHEN length(CAST(key_json AS BLOB))<=?3 THEN key_json END,CASE WHEN length(CAST(receipt_json AS BLOB))<=?3 THEN receipt_json END,length(CAST(key_json AS BLOB)),length(CAST(receipt_json AS BLOB)) FROM build_index WHERE category=?1 AND key_digest=?2",
            params![self.layout.category(), expected.key.digest, MAX_ENCODING as i64], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))).optional()?;
        self.check()?;
        let Some((version, key, receipt, key_length, receipt_length)) = row else { return Ok(None); };
        ensure!(key_length >= 0 && receipt_length >= 0 && key_length <= MAX_ENCODING as i64 && receipt_length <= MAX_ENCODING as i64,
            "retained index row exceeds preparation profile");
        let key = key.context("retained preparation key encoding absent")?;
        let receipt = receipt.context("retained preparation receipt encoding absent")?;
        ensure!(version == FORMAT_VERSION as i64 && serde_json::from_str::<ObjectRef>(&key)? == expected.key, "retained preparation index key mismatch");
        let receipt: ObjectRef = serde_json::from_str(&receipt)?;
        ensure!(receipt == expected.receipt, "same preparation key has a different root/receipt");
        Ok(Some(receipt))
    }
    fn run_inner(&mut self) -> Result<()> {
        self.encode()?; self.capture_index()?; self.key_lock()?; self.open_sqlite()?;
        self.status = RetainedPreparationStatus::Inspecting;
        let existing = self.lookup()?;
        self.store = Some(PreparationStore::capture(self.index.store.clone(), self.clock.clone()));
        self.store.as_mut().unwrap().acquire()?; self.check()?;
        let prepared = self.expected.as_ref().unwrap().clone(); let name = pin(self.layout.category(), &prepared.key);
        let covered = self.store.as_mut().unwrap().covered_inputs(&self.anchors)?;
        ensure!(covered.contains(&self.directory), "actual prepared directory lacks current retained anchor");
        let key_refs = crate::store::metadata_references(ObjectKind::ActionKey, &self.bytes.as_ref().unwrap()[0])?;
        ensure!(key_refs.iter().all(|r| covered.contains(r)), "preparation contract references unretained input");
        for tool in self.spec.toolchains.values() {
            self.check()?;
            ensure!(self.store.as_mut().unwrap().file_at(&self.directory, &tool.executable_path)? == tool.executable,
                "actual prepared executable differs from requested tool identity");
        }
        if existing.is_some() {
            ensure!(self.store.as_ref().unwrap().pin_matches(&name, &prepared.receipt)?, "existing prepared index lacks matching retention authority");
            // The row only names DATA. Verify all three actual descriptors and
            // original category/layout bytes before issuing a genuine hit ACK.
            for (i, r) in [&prepared.key, &prepared.root, &prepared.receipt].into_iter().enumerate() {
                ensure!(self.store.as_mut().unwrap().read(r)? == self.bytes.as_ref().unwrap()[i], "retained preparation descriptor changed");
            }
        } else {
            ensure!(!self.store.as_ref().unwrap().pin_present(&name)?,
                "unindexed preparation pin already exists; original recovery required");
            self.status = RetainedPreparationStatus::Staging;
            self.store.as_mut().unwrap().begin_staging()?;
            #[cfg(test)] self.point(TestPoint::AfterStaging)?;
            for (i, r) in [&prepared.key, &prepared.root, &prepared.receipt].into_iter().enumerate() {
                self.check()?; self.object_attempts = i + 1;
                self.store.as_mut().unwrap().put(r, &self.bytes.as_ref().unwrap()[i])?;
            }
            self.status = RetainedPreparationStatus::Publishing; self.pin_attempted = true;
            #[cfg(test)] if self.fault == Some(TestPoint::PinDirectorySync) { self.store.as_mut().unwrap().fail_after_pin_rename = true; }
            self.pin_generation = Some(self.store.as_mut().unwrap().publish(&name, &prepared.receipt, &covered)?);
            #[cfg(test)] self.point(TestPoint::AfterPin)?;
            self.check()?; self.status = RetainedPreparationStatus::Inserting; self.sql = SqlOutcome::InsertAttempted;
            self.connection.as_ref().unwrap().execute("INSERT INTO build_index(format_version,category,key_digest,key_json,receipt_json) VALUES(?1,?2,?3,?4,?5)",
                params![FORMAT_VERSION, self.layout.category(), prepared.key.digest, serde_json::to_string(&prepared.key)?, serde_json::to_string(&prepared.receipt)?])?;
            self.sql = SqlOutcome::Inserted;
            #[cfg(test)] self.point(TestPoint::AfterInsert)?;
        }
        self.check()?; self.status = RetainedPreparationStatus::Committing; self.sql = SqlOutcome::CommitAttempted;
        self.connection.as_ref().unwrap().execute_batch("COMMIT")?; self.sql = SqlOutcome::Committed;
        #[cfg(test)] self.point(TestPoint::AfterCommit)?;
        self.check()?;
        ensure!(self.lookup()? == Some(prepared.receipt.clone()), "original committed preparation index differs");
        self.store.as_mut().unwrap().verify_pin_current(&name, &prepared.receipt)?;
        self.check()?;
        // Only known synchronous completion releases ordinary owned locks.
        // No subsequent fallible cleanup can turn this into a replay path.
        self.store.as_mut().unwrap().release_known(); self.key_locked = false; self.key_file.take();
        self.ack = Some(PreparationAcknowledgment { prepared, reused: existing.is_some() });
        self.status = RetainedPreparationStatus::Acknowledged; Ok(())
    }
    #[cfg(test)] fn point(&self, point: TestPoint) -> Result<()> {
        if self.fault == Some(TestPoint::CancelAfterCommit) && point == TestPoint::AfterCommit { self.original.cancel(); }
        if self.fault == Some(TestPoint::CancelAfterStaging) && point == TestPoint::AfterStaging { self.original.cancel(); }
        ensure!(self.fault != Some(point), "injected original preparation outcome loss"); Ok(())
    }
}
fn sqlite_binding_current(connection: &Connection, expected: &Path) -> Result<()> {
    sqlite_not_moved(connection)?;
    let actual = connection.path().context("original preparation SQLite filename absent")?;
    ensure!(Path::new(actual).canonicalize()? == expected, "SQLite connection belongs to another preparation index");
    let named = fs::symlink_metadata(expected)?;
    ensure!(named.is_file() && !named.file_type().is_symlink() && named.nlink() == 1
        && named.uid() == unsafe { libc::geteuid() } && named.mode() & 0o022 == 0,
        "original preparation SQLite path not regular/private");
    sqlite_not_moved(connection)
}
fn sqlite_not_moved(connection: &Connection) -> Result<()> {
    let mut moved: libc::c_int = -1;
    let code = unsafe {
        rusqlite::ffi::sqlite3_file_control(connection.handle(), c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED, (&mut moved as *mut libc::c_int).cast())
    };
    ensure!(code == rusqlite::ffi::SQLITE_OK && moved == 0,
        "actual original SQLite preparation index moved or custody check unsupported");
    Ok(())
}
fn encode_metadata<T: Serialize>(schema: &str, value: &T, mut references: Vec<ObjectRef>) -> Result<Vec<u8>> {
    references.sort(); references.dedup();
    canonical_json(&ReferencedMetadata { format_version: FORMAT_VERSION, references,
        payload: serde_json::json!({"schema":schema,"value":value}) })
}
fn scalar_bound(total: &mut usize, value: &str) -> Result<()> {
    ensure!(value.len() <= 4096, "preparation scalar exceeds profile");
    *total = total.checked_add(value.len()).context("preparation scalar bound overflow")?;
    ensure!(*total <= MAX_ENCODING / 2, "preparation scalar total exceeds profile"); Ok(())
}

#[cfg(test)] #[derive(Clone, Copy, PartialEq, Eq)]
enum TestPoint { AfterStaging, PinDirectorySync, AfterPin, AfterInsert, AfterCommit, CancelAfterCommit, CancelAfterStaging }
#[cfg(target_os = "linux")]
#[path = "backed_preparation_index.rs"]
mod backed_index;

#[cfg(test)]
#[path = "preparation_retaining_tests.rs"]
mod tests;

#[cfg(target_os="linux")]
#[path="index_acquisition_impl.rs"]
mod index_acquisition_impl;

#[cfg(target_os="linux")]
#[path="environment_lineage_impl.rs"]
mod environment_lineage_impl;
