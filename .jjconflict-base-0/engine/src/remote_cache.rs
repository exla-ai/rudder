//! Reconstructible, scoped PARTIAL immutable objects, separate from LocalStore.
//!
//! Membership proves only an object's bytes/schema. It grants no root access,
//! complete-closure durability, pin or authorization. The trusted caller admits
//! only refs reached from its authorized retained remote root. No default VFS
//! or LocalStore semantics are changed here.
//!
//! request() performs bounded in-memory admission only. A fixed worker pool
//! handles filesystem/transport work; one fixed notifier handles deadlines.
//! Providers must honor FetchControl and stream at most TRANSFER_BUFFER_BYTES
//! per write. An ignoring provider keeps its worker, job and byte reservations;
//! cancellation never creates a replacement thread. Initialization and explicit
//! ticket.wait() are blocking APIs and belong outside engine/App actors.
//! Snapshot, borrowed, in-flight and encoded-disk quotas are independent. Disk
//! bytes account encoded payloads plus reservations, not filesystem overhead;
//! entry/job counts separately bound that overhead. Startup bounds inventory
//! scanning, removes recognized orphan temps and defers body/schema verification
//! of recovered entries until the first local read. Metadata admission proves
//! one object's local schema, never actual child edges or a complete root.

use crate::protocol::{canonical_json, HashAlgorithm, ObjectKind, ObjectRef};
use fs2::FileExt as LockFileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

pub const MAX_METADATA_BYTES: u64 = 64 * 1024;
pub const MAX_BLOB_BYTES: u64 = 4 * 1024 * 1024;
pub const TRANSFER_BUFFER_BYTES: usize = 64 * 1024;
type Result<T> = std::result::Result<T, CacheError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheError {
    Busy,
    TimedOut,
    Cancelled,
    Closed,
    InvalidReference,
    InvalidLimits,
    InvalidScope,
    UnsafePath,
    Corrupt,
    SourceFailed,
    Io,
    Poisoned,
}
impl CacheError {
    pub fn errno(self) -> i32 {
        match self {
            Self::Busy => libc::EAGAIN,
            Self::TimedOut => libc::ETIMEDOUT,
            Self::Cancelled | Self::Closed => libc::ECANCELED,
            Self::InvalidReference | Self::InvalidLimits | Self::InvalidScope => libc::EINVAL,
            _ => libc::EIO,
        }
    }
}
impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "immutable cache admission is busy",
            Self::TimedOut => "immutable cache request timed out",
            Self::Cancelled => "immutable cache request cancelled",
            Self::Closed => "immutable cache is closed",
            Self::InvalidReference => "object exceeds partial-cache reference/format limits",
            Self::InvalidLimits => "invalid immutable cache limits",
            Self::InvalidScope => "invalid immutable cache scope",
            Self::UnsafePath => "unsafe or unknown immutable cache path",
            Self::Corrupt => "immutable object failed verification",
            Self::SourceFailed => "immutable object source failed",
            Self::Io => "immutable cache I/O failed",
            Self::Poisoned => "immutable cache accounting is unavailable",
        })
    }
}
impl std::error::Error for CacheError {}
fn io<T>(value: std::io::Result<T>) -> Result<T> {
    value.map_err(|_| CacheError::Io)
}
fn lock<T>(value: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    value.lock().map_err(|_| CacheError::Poisoned)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheScope {
    pub provider_id: String,
    pub tenant_id: String,
}
impl CacheScope {
    pub fn digest(&self) -> Result<String> {
        for part in [&self.provider_id, &self.tenant_id] {
            if part.is_empty()
                || part.len() > 128
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
            {
                return Err(CacheError::InvalidScope);
            }
        }
        let bytes = canonical_json(&(1, self)).map_err(|_| CacheError::InvalidScope)?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }
}
/// Constructed/configured only by the trusted host. The source owns credentials
/// and account/provider binding; a guest cannot supply an endpoint or token.
pub trait ImmutableObjectSource: Send + Sync + 'static {
    fn scope(&self) -> CacheScope;
    fn fetch(
        &self,
        reference: &ObjectRef,
        control: &FetchControl,
        output: &mut dyn Write,
    ) -> anyhow::Result<()>;
}
#[derive(Clone)]
pub struct FetchControl {
    cancelled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    deadline: Instant,
}
impl FetchControl {
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    pub fn is_cancelled(&self) -> bool {
        self.check().is_err()
    }
    pub fn check(&self) -> Result<()> {
        if self.shutdown.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire) {
            Err(CacheError::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(CacheError::TimedOut)
        } else {
            Ok(())
        }
    }
}
#[derive(Debug, Clone)]
pub struct CacheLimits {
    pub workers: usize,
    pub max_jobs: usize,
    pub max_tickets: usize,
    pub max_inflight_bytes: u64,
    pub max_resident_bytes: u64,
    pub max_borrowed_bytes: u64,
    /// Encoded object bytes (minimum one per entry), including fetch reservations.
    /// Filesystem/inode overhead is separately bounded by entry/job counts.
    pub disk_quota_bytes: u64,
    pub max_disk_entries: usize,
    pub max_recovery_entries: usize,
    pub fetch_timeout: Duration,
}
impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            workers: 2,
            max_jobs: 8,
            max_tickets: 256,
            max_inflight_bytes: 16 * 1024 * 1024,
            max_resident_bytes: 32 * 1024 * 1024,
            max_borrowed_bytes: 16 * 1024 * 1024,
            disk_quota_bytes: 256 * 1024 * 1024,
            max_disk_entries: 4096,
            max_recovery_entries: 8192,
            fetch_timeout: Duration::from_secs(30),
        }
    }
}
impl CacheLimits {
    fn validate(&self) -> Result<()> {
        if self.workers == 0
            || self.workers > 16
            || self.max_jobs == 0
            || self.max_jobs > 1024
            || self.max_tickets == 0
            || self.max_tickets > 4096
            || self.max_inflight_bytes == 0
            || self.max_resident_bytes == 0
            || self.max_borrowed_bytes == 0
            || self.disk_quota_bytes == 0
            || self.max_disk_entries == 0
            || self.max_recovery_entries < self.max_disk_entries
            || self.max_recovery_entries > 1_000_000
            || self.fetch_timeout.is_zero()
            || self.fetch_timeout > Duration::from_secs(120)
        {
            Err(CacheError::InvalidLimits)
        } else {
            Ok(())
        }
    }
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct RecoveryReport {
    pub scanned_entries: u64,
    pub recovered_entries: u64,
    pub removed_orphan_temps: u64,
    pub removed_corrupt_entries: u64,
    pub evicted_entries: u64,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheStats {
    pub inflight_jobs: usize,
    pub running_jobs: usize,
    pub live_tickets: usize,
    pub inflight_bytes: u64,
    pub pending_resident_bytes: u64,
    pub resident_bytes: u64,
    pub borrowed_bytes: u64,
    pub disk_entries: usize,
    pub disk_bytes: u64,
    pub reserved_disk_bytes: u64,
    pub reserved_disk_entries: usize,
    pub remote_fetches: u64,
    pub verified_disk_hits: u64,
    pub resident_hits: u64,
    pub evictions: u64,
    pub corrupt_entries: u64,
    pub poisoned: bool,
    pub closed: bool,
}
struct DiskEntry {
    last_used: u64,
    resident: Weak<VerifiedEntry>,
    evicting: bool,
}
struct State {
    entries: BTreeMap<ObjectRef, DiskEntry>,
    jobs: BTreeMap<ObjectRef, Arc<Job>>,
    queue: VecDeque<Arc<Job>>,
    waiters: BTreeMap<u64, Weak<Waiter>>,
    next_ticket: u64,
    clock: u64,
    epoch: u64,
    stats: CacheStats,
}
struct Shared {
    root: PathBuf,
    scope: CacheScope,
    limits: CacheLimits,
    source: Arc<dyn ImmutableObjectSource>,
    state: Mutex<State>,
    work: Condvar,
    timer: Condvar,
    shutdown: Arc<AtomicBool>,
    _owner_lock: File,
    recovery: RecoveryReport,
}
struct Manager {
    shared: Arc<Shared>,
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.shared.close();
    }
}
#[derive(Clone)]
pub struct RemoteCache(Arc<Manager>);
struct Job {
    reference: ObjectRef,
    control: FetchControl,
    result: Mutex<Option<(Instant, Result<Arc<VerifiedEntry>>)>>,
    waiters: Mutex<BTreeMap<u64, Weak<Waiter>>>,
    interested: AtomicUsize,
}
struct Waiter {
    id: u64,
    job: Arc<Job>,
    shared: Arc<Shared>,
    deadline: Instant,
    cancelled: AtomicBool,
    timed_out: AtomicBool,
    interested: AtomicBool,
    waker: Mutex<Option<Waker>>,
    changed: Condvar,
}
pub struct CacheTicket {
    waiter: Arc<Waiter>,
}
struct VerifiedEntry {
    reference: ObjectRef,
    bytes: Vec<u8>,
    shared: Arc<Shared>,
    borrow: Mutex<Weak<BorrowLease>>,
    accounted: AtomicBool,
}
struct BorrowLease {
    shared: Arc<Shared>,
    cost: u64,
}
/// A verified immutable snapshot; no raw cache FD or writable path escapes.
/// Clones share one borrowed-byte charge for this entry.
#[derive(Clone)]
pub struct CachedObject {
    _borrow: Arc<BorrowLease>,
    entry: Arc<VerifiedEntry>,
}
impl std::fmt::Debug for CachedObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedObject")
            .field("reference", &self.entry.reference)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for CacheTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheTicket")
            .field("reference", &self.waiter.job.reference)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for RemoteCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteCache")
            .field("namespace", &self.0.shared.root)
            .finish_non_exhaustive()
    }
}
impl CachedObject {
    pub fn reference(&self) -> &ObjectRef {
        &self.entry.reference
    }
    pub fn bytes(&self) -> &[u8] {
        &self.entry.bytes
    }
    pub fn slice(&self, offset: u64, length: usize) -> Result<&[u8]> {
        let start = usize::try_from(offset).map_err(|_| CacheError::InvalidReference)?;
        let end = start
            .checked_add(length)
            .ok_or(CacheError::InvalidReference)?;
        self.bytes()
            .get(start..end)
            .ok_or(CacheError::InvalidReference)
    }
}
impl Drop for VerifiedEntry {
    fn drop(&mut self) {
        if self.accounted.swap(false, Ordering::AcqRel) {
            if let Ok(mut state) = self.shared.state.lock() {
                if let Some(next) = state
                    .stats
                    .resident_bytes
                    .checked_sub(cost(&self.reference))
                {
                    state.stats.resident_bytes = next;
                } else {
                    state.stats.poisoned = true;
                }
            }
        }
    }
}
impl Drop for BorrowLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.stats.borrowed_bytes = state.stats.borrowed_bytes.saturating_sub(self.cost);
        }
    }
}
impl VerifiedEntry {
    fn borrowed(self: &Arc<Self>) -> Result<CachedObject> {
        let mut borrow = lock(&self.borrow)?;
        let lease = if let Some(existing) = borrow.upgrade() {
            existing
        } else {
            let amount = cost(&self.reference);
            let mut state = lock(&self.shared.state)?;
            let next = state
                .stats
                .borrowed_bytes
                .checked_add(amount)
                .ok_or(CacheError::Busy)?;
            if next > self.shared.limits.max_borrowed_bytes {
                return Err(CacheError::Busy);
            }
            state.stats.borrowed_bytes = next;
            let lease = Arc::new(BorrowLease {
                shared: self.shared.clone(),
                cost: amount,
            });
            *borrow = Arc::downgrade(&lease);
            lease
        };
        Ok(CachedObject {
            _borrow: lease,
            entry: self.clone(),
        })
    }
}
fn cost(reference: &ObjectRef) -> u64 {
    reference.size_bytes.max(1)
}
fn reference_ok(reference: &ObjectRef) -> Result<()> {
    reference
        .validate()
        .map_err(|_| CacheError::InvalidReference)?;
    let cap = if reference.kind == ObjectKind::Blob {
        MAX_BLOB_BYTES
    } else {
        MAX_METADATA_BYTES
    };
    if reference.size_bytes > cap {
        return Err(CacheError::InvalidReference);
    }
    Ok(())
}
fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::File => "file",
        ObjectKind::Directory => "directory",
        ObjectKind::DirectoryShard => "directory_shard",
        ObjectKind::SourceRoot => "source_root",
        ObjectKind::EnvironmentRoot => "environment_root",
        ObjectKind::Checkpoint => "checkpoint",
        ObjectKind::RunnableRoot => "runnable_root",
        ObjectKind::ActionKey => "action_key",
        ObjectKind::ActionResult => "action_result",
    }
}
fn file_name(reference: &ObjectRef) -> String {
    format!(
        "{}-{}-{}.obj",
        kind_name(reference.kind),
        reference.digest,
        reference.size_bytes
    )
}
fn parse_name(name: &str) -> Result<ObjectRef> {
    let stem = name.strip_suffix(".obj").ok_or(CacheError::UnsafePath)?;
    let (prefix, size) = stem.rsplit_once('-').ok_or(CacheError::UnsafePath)?;
    let (kind, digest) = prefix.rsplit_once('-').ok_or(CacheError::UnsafePath)?;
    let kind = [
        ObjectKind::Blob,
        ObjectKind::File,
        ObjectKind::Directory,
        ObjectKind::DirectoryShard,
        ObjectKind::SourceRoot,
        ObjectKind::EnvironmentRoot,
        ObjectKind::Checkpoint,
        ObjectKind::RunnableRoot,
        ObjectKind::ActionKey,
        ObjectKind::ActionResult,
    ]
    .into_iter()
    .find(|candidate| kind_name(*candidate) == kind)
    .ok_or(CacheError::UnsafePath)?;
    let reference = ObjectRef {
        kind,
        algorithm: HashAlgorithm::Sha256,
        digest: digest.into(),
        size_bytes: size.parse().map_err(|_| CacheError::UnsafePath)?,
    };
    reference_ok(&reference).map_err(|_| CacheError::UnsafePath)?;
    if file_name(&reference) != name {
        return Err(CacheError::UnsafePath);
    }
    Ok(reference)
}
fn regular(path: &Path, write: bool) -> Result<File> {
    let before = io(fs::symlink_metadata(path))?;
    if !before.is_file()
        || before.file_type().is_symlink()
        || before.nlink() != 1
        || before.uid() != unsafe { libc::geteuid() }
    {
        return Err(CacheError::UnsafePath);
    }
    let file = io(OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path))?;
    let metadata = io(file.metadata())?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(CacheError::UnsafePath);
    }
    Ok(file)
}
fn sync_dir(path: &Path) -> Result<()> {
    io(File::open(path))?.sync_all().map_err(|_| CacheError::Io)
}
fn private_dir(path: &Path, create: bool) -> Result<()> {
    if create {
        match fs::create_dir(path) {
            Ok(()) => {
                io(fs::set_permissions(path, fs::Permissions::from_mode(0o700)))?;
                sync_dir(path.parent().ok_or(CacheError::UnsafePath)?)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(CacheError::Io),
        }
    }
    let metadata = io(fs::symlink_metadata(path))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(CacheError::UnsafePath);
    }
    Ok(())
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Marker {
    format_version: u32,
    cache_kind: String,
    scope_digest: String,
}
fn prepare_root(
    base: &Path,
    scope: &CacheScope,
    limits: &CacheLimits,
) -> Result<(
    PathBuf,
    File,
    BTreeMap<ObjectRef, DiskEntry>,
    RecoveryReport,
    u64,
)> {
    if !base.is_absolute() {
        return Err(CacheError::UnsafePath);
    }
    let metadata = io(fs::symlink_metadata(base))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(CacheError::UnsafePath);
    }
    let digest = scope.digest()?;
    let root = base.join(format!("rudder-partial-v1-{digest}"));
    private_dir(&root, true)?;
    let marker = root.join("cache.json");
    if !marker.exists() {
        for (index, entry) in io(fs::read_dir(&root))?.enumerate() {
            if index >= limits.max_recovery_entries.saturating_add(4) {
                return Err(CacheError::UnsafePath);
            }
            let entry = io(entry)?;
            let name = entry.file_name();
            let name = name.to_str().ok_or(CacheError::UnsafePath)?;
            if name != "lock" && !marker_temp(name) {
                return Err(CacheError::UnsafePath);
            }
        }
    }
    let owner = io(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.join("lock")))?;
    let metadata = io(owner.metadata())?;
    if metadata.len() != 0 || metadata.mode() & 0o777 != 0o600 {
        return Err(CacheError::UnsafePath);
    }
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(CacheError::UnsafePath);
    }
    owner.try_lock_exclusive().map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            CacheError::Busy
        } else {
            CacheError::Io
        }
    })?;
    let expected = Marker {
        format_version: 1,
        cache_kind: "rudder_partial_immutable_objects".into(),
        scope_digest: digest,
    };
    if marker.exists() {
        let file = regular(&marker, false)?;
        if io(file.metadata())?.len() > 4096 {
            return Err(CacheError::UnsafePath);
        }
        let mut bytes = Vec::new();
        io(file.take(4097).read_to_end(&mut bytes))?;
        let parsed: Marker = serde_json::from_slice(&bytes).map_err(|_| CacheError::UnsafePath)?;
        if parsed != expected
            || canonical_json(&parsed).map_err(|_| CacheError::UnsafePath)? != bytes
        {
            return Err(CacheError::UnsafePath);
        }
    } else {
        let path = root.join(format!("marker-{}.tmp", Uuid::new_v4()));
        let mut file = io(OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path))?;
        io(file.write_all(&canonical_json(&expected).map_err(|_| CacheError::UnsafePath)?))?;
        io(file.sync_all())?;
        io(fs::rename(&path, &marker))?;
        sync_dir(&root)?;
    }
    for (index, entry) in io(fs::read_dir(&root))?.enumerate() {
        if index >= limits.max_recovery_entries.saturating_add(4) {
            return Err(CacheError::UnsafePath);
        }
        let entry = io(entry)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(CacheError::UnsafePath)?;
        if !["cache.json", "lock", "entries", "tmp"].contains(&name) && !marker_temp(name) {
            return Err(CacheError::UnsafePath);
        }
        if io(fs::symlink_metadata(entry.path()))?
            .file_type()
            .is_symlink()
        {
            return Err(CacheError::UnsafePath);
        }
    }
    private_dir(&root.join("entries"), true)?;
    private_dir(&root.join("tmp"), true)?;
    let mut report = RecoveryReport::default();
    let mut candidates = Vec::new();
    let mut remove = Vec::new();
    let mut scanned = 0usize;
    for entry in io(fs::read_dir(root.join("entries")))? {
        let entry = io(entry)?;
        scanned = scanned.checked_add(1).ok_or(CacheError::UnsafePath)?;
        if scanned > limits.max_recovery_entries {
            return Err(CacheError::UnsafePath);
        }
        let name = entry.file_name();
        let reference = parse_name(name.to_str().ok_or(CacheError::UnsafePath)?)?;
        let file = regular(&entry.path(), false)?;
        let metadata = io(file.metadata())?;
        report.scanned_entries += 1;
        if metadata.len() != reference.size_bytes || metadata.mode() & 0o777 != 0o444 {
            remove.push(entry.path());
            report.removed_corrupt_entries += 1;
        } else {
            candidates.push((
                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                reference,
            ));
        }
    }
    let mut orphans = Vec::new();
    for entry in io(fs::read_dir(root.join("tmp")))? {
        let entry = io(entry)?;
        scanned = scanned.checked_add(1).ok_or(CacheError::UnsafePath)?;
        if scanned > limits.max_recovery_entries {
            return Err(CacheError::UnsafePath);
        }
        let name = entry.file_name();
        if !part_name(name.to_str().ok_or(CacheError::UnsafePath)?) {
            return Err(CacheError::UnsafePath);
        }
        regular(&entry.path(), false)?;
        orphans.push(entry.path());
    }
    for (index, entry) in io(fs::read_dir(&root))?.enumerate() {
        if index >= limits.max_recovery_entries.saturating_add(4) {
            return Err(CacheError::UnsafePath);
        }
        let entry = io(entry)?;
        if marker_temp(entry.file_name().to_str().ok_or(CacheError::UnsafePath)?) {
            regular(&entry.path(), false)?;
            orphans.push(entry.path());
        }
    }
    candidates.sort();
    let mut candidates = VecDeque::from(candidates);
    let mut disk_bytes = 0u64;
    for (_, reference) in &candidates {
        disk_bytes = disk_bytes
            .checked_add(cost(reference))
            .ok_or(CacheError::UnsafePath)?;
    }
    while candidates.len() > limits.max_disk_entries || disk_bytes > limits.disk_quota_bytes {
        let (_, reference) = candidates.pop_front().ok_or(CacheError::Poisoned)?;
        disk_bytes -= cost(&reference);
        remove.push(root.join("entries").join(file_name(&reference)));
        report.evicted_entries += 1;
    }
    for path in remove {
        io(fs::remove_file(path))?;
    }
    for path in orphans {
        io(fs::remove_file(path))?;
        report.removed_orphan_temps += 1;
    }
    sync_dir(&root.join("entries"))?;
    sync_dir(&root.join("tmp"))?;
    sync_dir(&root)?;
    let entries = candidates
        .into_iter()
        .enumerate()
        .map(|(index, (_, reference))| {
            (
                reference,
                DiskEntry {
                    last_used: index as u64,
                    resident: Weak::new(),
                    evicting: false,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    report.recovered_entries = entries.len() as u64;
    Ok((root, owner, entries, report, disk_bytes))
}
fn marker_temp(name: &str) -> bool {
    name.strip_prefix("marker-")
        .and_then(|s| s.strip_suffix(".tmp"))
        .is_some_and(|s| Uuid::parse_str(s).is_ok_and(|id| id.to_string() == s))
}
fn part_name(name: &str) -> bool {
    name.strip_prefix("part-")
        .and_then(|s| s.strip_suffix(".tmp"))
        .is_some_and(|s| Uuid::parse_str(s).is_ok_and(|id| id.to_string() == s))
}

impl RemoteCache {
    /// Blocking startup/recovery. base must already be a trusted directory.
    pub fn open(
        base: impl AsRef<Path>,
        source: Arc<dyn ImmutableObjectSource>,
        limits: CacheLimits,
    ) -> Result<Self> {
        limits.validate()?;
        let scope = source.scope();
        let (root, owner, entries, recovery, disk_bytes) =
            prepare_root(base.as_ref(), &scope, &limits)?;
        let count = entries.len();
        let stats = CacheStats {
            disk_entries: count,
            disk_bytes,
            ..CacheStats::default()
        };
        let shared = Arc::new(Shared {
            root,
            scope,
            limits: limits.clone(),
            source,
            state: Mutex::new(State {
                entries,
                jobs: BTreeMap::new(),
                queue: VecDeque::new(),
                waiters: BTreeMap::new(),
                next_ticket: 1,
                clock: count as u64,
                epoch: 0,
                stats,
            }),
            work: Condvar::new(),
            timer: Condvar::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            _owner_lock: owner,
            recovery,
        });
        for index in 0..limits.workers {
            let worker = shared.clone();
            if std::thread::Builder::new()
                .name(format!("rudder-cache-{index}"))
                .spawn(move || worker_loop(worker))
                .is_err()
            {
                shared.close();
                return Err(CacheError::Io);
            }
        }
        let notifier = shared.clone();
        if std::thread::Builder::new()
            .name("rudder-cache-clock".into())
            .spawn(move || deadline_loop(notifier))
            .is_err()
        {
            shared.close();
            return Err(CacheError::Io);
        }
        Ok(Self(Arc::new(Manager { shared })))
    }
    pub fn namespace_root(&self) -> &Path {
        &self.0.shared.root
    }
    pub fn scope(&self) -> &CacheScope {
        &self.0.shared.scope
    }
    pub fn recovery(&self) -> &RecoveryReport {
        &self.0.shared.recovery
    }
    pub fn stats(&self) -> Result<CacheStats> {
        let mut stats = lock(&self.0.shared.state)?.stats.clone();
        stats.closed = self.0.shared.shutdown.load(Ordering::Acquire);
        Ok(stats)
    }
    pub fn shutdown(&self) {
        self.0.shared.close();
    }
    /// Accepted work is not completed data. Only a successful ticket result
    /// yields a verified immutable snapshot. This method performs no FS/network IO.
    pub fn request(&self, reference: &ObjectRef, deadline: Instant) -> Result<CacheTicket> {
        reference_ok(reference)?;
        if deadline <= Instant::now() {
            return Err(CacheError::TimedOut);
        }
        let shared = &self.0.shared;
        let mut state = lock(&shared.state)?;
        if shared.shutdown.load(Ordering::Acquire) {
            return Err(CacheError::Closed);
        }
        if state.stats.poisoned {
            return Err(CacheError::Poisoned);
        }
        if state.waiters.len() >= shared.limits.max_tickets {
            return Err(CacheError::Busy);
        }
        let id = state.next_ticket;
        let next_ticket = state.next_ticket.checked_add(1).ok_or(CacheError::Busy)?;
        let amount = cost(reference);
        if amount > shared.limits.max_borrowed_bytes {
            return Err(CacheError::Busy);
        }
        state.clock = state.clock.saturating_add(1);
        let clock = state.clock;
        let resident = if let Some(entry) = state.entries.get_mut(reference) {
            if entry.evicting {
                return Err(CacheError::Busy);
            }
            entry.last_used = clock;
            entry.resident.upgrade()
        } else {
            None
        };
        let job = if let Some(resident) = resident {
            state.stats.resident_hits = state.stats.resident_hits.saturating_add(1);
            Arc::new(Job {
                reference: reference.clone(),
                control: FetchControl {
                    cancelled: Arc::new(AtomicBool::new(false)),
                    shutdown: shared.shutdown.clone(),
                    deadline,
                },
                result: Mutex::new(Some((Instant::now(), Ok(resident)))),
                waiters: Mutex::new(BTreeMap::new()),
                interested: AtomicUsize::new(0),
            })
        } else if let Some(job) = state.jobs.get(reference) {
            if job.control.cancelled.load(Ordering::Acquire)
                || Instant::now() >= job.control.deadline
            {
                return Err(CacheError::Busy);
            }
            job.clone()
        } else {
            let next_bytes = state
                .stats
                .inflight_bytes
                .checked_add(amount)
                .ok_or(CacheError::Busy)?;
            let memory = state
                .stats
                .resident_bytes
                .checked_add(state.stats.pending_resident_bytes)
                .and_then(|n| n.checked_add(amount))
                .ok_or(CacheError::Busy)?;
            if state.jobs.len() >= shared.limits.max_jobs
                || next_bytes > shared.limits.max_inflight_bytes
                || memory > shared.limits.max_resident_bytes
            {
                return Err(CacheError::Busy);
            }
            let timeout = Instant::now()
                .checked_add(shared.limits.fetch_timeout)
                .ok_or(CacheError::InvalidLimits)?;
            let job = Arc::new(Job {
                reference: reference.clone(),
                control: FetchControl {
                    cancelled: Arc::new(AtomicBool::new(false)),
                    shutdown: shared.shutdown.clone(),
                    deadline: deadline.min(timeout),
                },
                result: Mutex::new(None),
                waiters: Mutex::new(BTreeMap::new()),
                interested: AtomicUsize::new(0),
            });
            state.stats.inflight_bytes = next_bytes;
            state.stats.pending_resident_bytes += amount;
            state.jobs.insert(reference.clone(), job.clone());
            state.stats.inflight_jobs = state.jobs.len();
            state.queue.push_back(job.clone());
            shared.work.notify_one();
            job
        };
        state.next_ticket = next_ticket;
        state.epoch = state.epoch.wrapping_add(1);
        let waiter = Arc::new(Waiter {
            id,
            job: job.clone(),
            shared: shared.clone(),
            deadline,
            cancelled: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            interested: AtomicBool::new(true),
            waker: Mutex::new(None),
            changed: Condvar::new(),
        });
        job.interested.fetch_add(1, Ordering::AcqRel);
        lock(&job.waiters)?.insert(id, Arc::downgrade(&waiter));
        state.waiters.insert(id, Arc::downgrade(&waiter));
        state.stats.live_tickets = state.waiters.len();
        shared.timer.notify_one();
        Ok(CacheTicket { waiter })
    }
}

impl Shared {
    fn close(&self) {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        let waiters = if let Ok(state) = self.state.lock() {
            for job in state.jobs.values() {
                job.control.cancelled.store(true, Ordering::Release);
            }
            state
                .waiters
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for waiter in waiters {
            waiter.notify();
        }
        self.work.notify_all();
        self.timer.notify_all();
    }
}
impl Waiter {
    fn effective_deadline(&self) -> Instant {
        self.deadline.min(self.job.control.deadline)
    }
    fn uninterested(&self) {
        if self.interested.swap(false, Ordering::AcqRel)
            && self.job.interested.fetch_sub(1, Ordering::AcqRel) == 1
        {
            self.job.control.cancelled.store(true, Ordering::Release);
        }
    }
    fn notify(&self) {
        let waker = self.waker.lock().ok().and_then(|mut waker| waker.take());
        if let Some(waker) = waker {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake()));
        }
        self.changed.notify_all();
    }
    fn expire(&self) {
        if Instant::now() < self.effective_deadline() {
            return;
        }
        let completed = self
            .job
            .result
            .lock()
            .ok()
            .and_then(|result| result.as_ref().map(|(when, _)| *when));
        if completed.is_some_and(|when| when <= self.effective_deadline()) {
            return;
        }
        self.timed_out.store(true, Ordering::Release);
        self.uninterested();
        self.notify();
    }
    fn result(&self) -> Option<Result<CachedObject>> {
        if self.cancelled.load(Ordering::Acquire) {
            return Some(Err(CacheError::Cancelled));
        }
        self.expire();
        if self.timed_out.load(Ordering::Acquire) {
            return Some(Err(CacheError::TimedOut));
        }
        match lock(&self.job.result) {
            Err(error) => Some(Err(error)),
            Ok(result) => match result.as_ref() {
                Some((_, Ok(entry))) => Some(entry.borrowed()),
                Some((_, Err(error))) => Some(Err(*error)),
                None => {
                    if self.shared.shutdown.load(Ordering::Acquire) {
                        Some(Err(CacheError::Cancelled))
                    } else {
                        None
                    }
                }
            },
        }
    }
}
impl Drop for Waiter {
    fn drop(&mut self) {
        self.uninterested();
        if let Ok(mut waiters) = self.job.waiters.lock() {
            waiters.remove(&self.id);
        }
        if let Ok(mut state) = self.shared.state.lock() {
            state.waiters.remove(&self.id);
            state.stats.live_tickets = state.waiters.len();
            state.epoch = state.epoch.wrapping_add(1);
        }
        self.shared.timer.notify_one();
    }
}
impl CacheTicket {
    pub fn try_result(&self) -> Option<Result<CachedObject>> {
        self.waiter.result()
    }
    pub fn cancel(&self) {
        self.waiter.cancelled.store(true, Ordering::Release);
        self.waiter.uninterested();
        self.waiter.notify();
    }
    /// Blocking convenience for a dedicated caller/IO worker, never an actor.
    pub fn wait(&self) -> Result<CachedObject> {
        loop {
            if let Some(result) = self.try_result() {
                return result;
            }
            let guard = lock(&self.waiter.waker)?;
            let remaining = self
                .waiter
                .effective_deadline()
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(10));
            drop(
                self.waiter
                    .changed
                    .wait_timeout(guard, remaining)
                    .map_err(|_| CacheError::Poisoned)?,
            );
        }
    }
}
impl Future for CacheTicket {
    type Output = Result<CachedObject>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Ok(mut waker) = self.waiter.waker.lock() {
            *waker = Some(cx.waker().clone());
        } else {
            return Poll::Ready(Err(CacheError::Poisoned));
        }
        match self.try_result() {
            Some(result) => Poll::Ready(result),
            None => Poll::Pending,
        }
    }
}

fn deadline_loop(shared: Arc<Shared>) {
    loop {
        let state = match shared.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if shared.shutdown.load(Ordering::Acquire) {
            return;
        }
        let epoch = state.epoch;
        let waiters = state
            .waiters
            .values()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        drop(state);
        let mut duration = Duration::from_secs(3600);
        for waiter in &waiters {
            waiter.expire();
            let pending = waiter
                .job
                .result
                .lock()
                .map(|r| r.is_none())
                .unwrap_or(false);
            if pending
                && !waiter.timed_out.load(Ordering::Acquire)
                && !waiter.cancelled.load(Ordering::Acquire)
            {
                duration = duration.min(
                    waiter
                        .effective_deadline()
                        .saturating_duration_since(Instant::now()),
                );
            }
        }
        drop(waiters);
        let state = match shared.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if shared.shutdown.load(Ordering::Acquire) {
            return;
        }
        if state.epoch != epoch {
            continue;
        }
        if shared
            .timer
            .wait_timeout(state, duration.max(Duration::from_millis(1)))
            .is_err()
        {
            return;
        }
    }
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        let job = {
            let mut state = match shared.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            loop {
                if shared.shutdown.load(Ordering::Acquire) {
                    return;
                }
                if let Some(job) = state.queue.pop_front() {
                    state.stats.running_jobs += 1;
                    break job;
                }
                state = match shared.work.wait(state) {
                    Ok(state) => state,
                    Err(_) => return,
                };
            }
        };
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| perform_job(&shared, &job)))
                .unwrap_or_else(|_| {
                    poison(&shared);
                    Err(CacheError::Poisoned)
                });
        finish_job(&shared, &job, result);
    }
}

struct Completed {
    bytes: Vec<u8>,
    disk_reserved: bool,
}
fn object_path(shared: &Shared, reference: &ObjectRef) -> PathBuf {
    shared.root.join("entries").join(file_name(reference))
}
fn poison(shared: &Shared) {
    if let Ok(mut state) = shared.state.lock() {
        state.stats.poisoned = true;
    }
}
fn remove_entry(shared: &Shared, reference: &ObjectRef) -> Result<()> {
    match fs::remove_file(object_path(shared, reference)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            poison(shared);
            return Err(CacheError::Io);
        }
    }
    if let Err(error) = sync_dir(&shared.root.join("entries")) {
        poison(shared);
        return Err(error);
    }
    let mut state = lock(&shared.state)?;
    if state.entries.remove(reference).is_some() {
        state.stats.disk_bytes = state
            .stats
            .disk_bytes
            .checked_sub(cost(reference))
            .ok_or(CacheError::Poisoned)?;
        state.stats.disk_entries = state.entries.len();
    }
    Ok(())
}
fn reserve_disk(shared: &Shared, job: &Job) -> Result<()> {
    let amount = cost(&job.reference);
    if amount > shared.limits.disk_quota_bytes {
        return Err(CacheError::Busy);
    }
    loop {
        job.control.check()?;
        let victim = {
            let mut state = lock(&shared.state)?;
            if state.stats.poisoned {
                return Err(CacheError::Poisoned);
            }
            let bytes = state
                .stats
                .disk_bytes
                .checked_add(state.stats.reserved_disk_bytes)
                .and_then(|bytes| bytes.checked_add(amount))
                .ok_or(CacheError::Busy)?;
            let count = state
                .entries
                .len()
                .checked_add(state.stats.reserved_disk_entries)
                .and_then(|n| n.checked_add(1))
                .ok_or(CacheError::Busy)?;
            if bytes <= shared.limits.disk_quota_bytes && count <= shared.limits.max_disk_entries {
                state.stats.reserved_disk_bytes = state
                    .stats
                    .reserved_disk_bytes
                    .checked_add(amount)
                    .ok_or(CacheError::Busy)?;
                state.stats.reserved_disk_entries += 1;
                return Ok(());
            }
            let victim = state
                .entries
                .iter()
                .filter(|(reference, entry)| {
                    !entry.evicting
                        && entry.resident.strong_count() == 0
                        && !state.jobs.contains_key(*reference)
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(reference, _)| reference.clone())
                .ok_or(CacheError::Busy)?;
            state
                .entries
                .get_mut(&victim)
                .ok_or(CacheError::Poisoned)?
                .evicting = true;
            victim
        };
        remove_entry(shared, &victim)?;
        let mut state = lock(&shared.state)?;
        state.stats.evictions = state.stats.evictions.saturating_add(1);
    }
}
fn release_disk_reservation(shared: &Shared, reference: &ObjectRef) {
    if let Ok(mut state) = shared.state.lock() {
        match (
            state.stats.reserved_disk_bytes.checked_sub(cost(reference)),
            state.stats.reserved_disk_entries.checked_sub(1),
        ) {
            (Some(bytes), Some(entries)) => {
                state.stats.reserved_disk_bytes = bytes;
                state.stats.reserved_disk_entries = entries;
            }
            _ => state.stats.poisoned = true,
        }
    }
}
fn verify_bytes(reference: &ObjectRef, bytes: &[u8]) -> Result<()> {
    reference.verify(bytes).map_err(|_| CacheError::Corrupt)?;
    if reference.kind != ObjectKind::Blob {
        crate::store::metadata_references(reference.kind, bytes)
            .map_err(|_| CacheError::Corrupt)?;
    }
    Ok(())
}
fn read_verified(
    file: &mut File,
    reference: &ObjectRef,
    control: &FetchControl,
) -> Result<Vec<u8>> {
    control.check()?;
    let metadata = io(file.metadata())?;
    if !metadata.is_file() || metadata.len() != reference.size_bytes {
        return Err(CacheError::Corrupt);
    }
    let mut bytes = vec![0; reference.size_bytes as usize];
    let mut offset = 0;
    while offset < bytes.len() {
        control.check()?;
        let end = (offset + TRANSFER_BUFFER_BYTES).min(bytes.len());
        io(file.read_exact(&mut bytes[offset..end]))?;
        offset = end;
    }
    let mut extra = [0];
    if io(file.read(&mut extra))? != 0 {
        return Err(CacheError::Corrupt);
    }
    verify_bytes(reference, &bytes)?;
    control.check()?;
    Ok(bytes)
}
struct Sink<'a> {
    file: &'a mut File,
    control: &'a FetchControl,
    expected: u64,
    written: u64,
    failure: Option<CacheError>,
}
impl Write for Sink<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let result = (|| -> Result<usize> {
            self.control.check()?;
            let total = self
                .written
                .checked_add(bytes.len() as u64)
                .ok_or(CacheError::Corrupt)?;
            if bytes.len() > TRANSFER_BUFFER_BYTES || total > self.expected {
                return Err(CacheError::Corrupt);
            }
            io(self.file.write_all(bytes))?;
            self.written = total;
            Ok(bytes.len())
        })();
        match result {
            Ok(length) => Ok(length),
            Err(error) => {
                self.failure = Some(error);
                Err(std::io::Error::other(
                    "bounded immutable source write failed",
                ))
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
fn fetch_new(shared: &Shared, job: &Job) -> Result<Completed> {
    reserve_disk(shared, job)?;
    let path = shared
        .root
        .join("tmp")
        .join(format!("part-{}.tmp", Uuid::new_v4()));
    let destination = object_path(shared, &job.reference);
    let mut published = false;
    let outcome = (|| -> Result<Completed> {
        job.control.check()?;
        let mut file = io(OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path))?;
        let mut sink = Sink {
            file: &mut file,
            control: &job.control,
            expected: job.reference.size_bytes,
            written: 0,
            failure: None,
        };
        {
            let mut state = lock(&shared.state)?;
            state.stats.remote_fetches = state.stats.remote_fetches.saturating_add(1);
        }
        let fetched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shared.source.fetch(&job.reference, &job.control, &mut sink)
        }));
        if let Some(error) = sink.failure {
            return Err(error);
        }
        job.control.check()?;
        match fetched {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                // Trusted bounded transports can distinguish retryable
                // admission/deadline/cancellation from provider failure. Never
                // forward provider strings or other internal error variants.
                return Err(match error.downcast_ref::<CacheError>() {
                    Some(CacheError::Busy) => CacheError::Busy,
                    Some(CacheError::TimedOut) => CacheError::TimedOut,
                    Some(CacheError::Cancelled) => CacheError::Cancelled,
                    _ => CacheError::SourceFailed,
                });
            }
            Err(_) => return Err(CacheError::SourceFailed),
        }
        if sink.written != job.reference.size_bytes {
            return Err(CacheError::Corrupt);
        }
        drop(sink);
        use std::io::{Seek, SeekFrom};
        io(file.seek(SeekFrom::Start(0)))?;
        let bytes = read_verified(&mut file, &job.reference, &job.control)?;
        io(file.set_permissions(fs::Permissions::from_mode(0o444)))?;
        io(file.sync_all())?;
        job.control.check()?;
        if fs::symlink_metadata(&destination).is_ok() {
            poison(shared);
            return Err(CacheError::UnsafePath);
        }
        io(fs::rename(&path, &destination))?;
        published = true;
        sync_dir(&shared.root.join("entries"))?;
        sync_dir(&shared.root.join("tmp"))?;
        Ok(Completed {
            bytes,
            disk_reserved: true,
        })
    })();
    if outcome.is_err() {
        if published {
            if fs::remove_file(&destination).is_err()
                || sync_dir(&shared.root.join("entries")).is_err()
            {
                poison(shared);
            }
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                if sync_dir(&shared.root.join("tmp")).is_err() {
                    poison(shared)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => poison(shared),
        }
        release_disk_reservation(shared, &job.reference);
    }
    outcome
}
fn perform_job(shared: &Shared, job: &Job) -> Result<Completed> {
    job.control.check()?;
    let present = lock(&shared.state)?.entries.contains_key(&job.reference);
    if present {
        let path = object_path(shared, &job.reference);
        let result = (|| -> Result<Vec<u8>> {
            let mut file = regular(&path, false)?;
            if io(file.metadata())?.mode() & 0o777 != 0o444 {
                return Err(CacheError::Corrupt);
            }
            read_verified(&mut file, &job.reference, &job.control)
        })();
        match result {
            Ok(bytes) => {
                let mut state = lock(&shared.state)?;
                state.stats.verified_disk_hits = state.stats.verified_disk_hits.saturating_add(1);
                return Ok(Completed {
                    bytes,
                    disk_reserved: false,
                });
            }
            Err(CacheError::TimedOut | CacheError::Cancelled) => {
                return result.map(|bytes| Completed {
                    bytes,
                    disk_reserved: false,
                })
            }
            Err(CacheError::UnsafePath) => {
                poison(shared);
                return Err(CacheError::UnsafePath);
            }
            Err(_) => {
                remove_entry(shared, &job.reference)?;
                let mut state = lock(&shared.state)?;
                state.stats.corrupt_entries = state.stats.corrupt_entries.saturating_add(1);
            }
        }
    }
    fetch_new(shared, job)
}
fn finish_job(shared: &Arc<Shared>, job: &Arc<Job>, outcome: Result<Completed>) {
    let amount = cost(&job.reference);
    let result = (|| -> Result<Arc<VerifiedEntry>> {
        let mut state = lock(&shared.state)?;
        let inflight = state
            .stats
            .inflight_bytes
            .checked_sub(amount)
            .ok_or(CacheError::Poisoned)?;
        let pending = state
            .stats
            .pending_resident_bytes
            .checked_sub(amount)
            .ok_or(CacheError::Poisoned)?;
        let running = state
            .stats
            .running_jobs
            .checked_sub(1)
            .ok_or(CacheError::Poisoned)?;
        let result = match outcome {
            Err(error) => Err(error),
            Ok(completed) => {
                let resident = state
                    .stats
                    .resident_bytes
                    .checked_add(amount)
                    .ok_or(CacheError::Poisoned)?;
                if resident
                    .checked_add(pending)
                    .is_none_or(|total| total > shared.limits.max_resident_bytes)
                {
                    return Err(CacheError::Poisoned);
                }
                let entry = Arc::new(VerifiedEntry {
                    reference: job.reference.clone(),
                    bytes: completed.bytes,
                    shared: shared.clone(),
                    borrow: Mutex::new(Weak::new()),
                    accounted: AtomicBool::new(false),
                });
                if completed.disk_reserved {
                    let disk_reserved = state
                        .stats
                        .reserved_disk_bytes
                        .checked_sub(amount)
                        .ok_or(CacheError::Poisoned)?;
                    let entries_reserved = state
                        .stats
                        .reserved_disk_entries
                        .checked_sub(1)
                        .ok_or(CacheError::Poisoned)?;
                    let disk = state
                        .stats
                        .disk_bytes
                        .checked_add(amount)
                        .ok_or(CacheError::Poisoned)?;
                    let clock = state.clock;
                    state.entries.insert(
                        job.reference.clone(),
                        DiskEntry {
                            last_used: clock,
                            resident: Arc::downgrade(&entry),
                            evicting: false,
                        },
                    );
                    state.stats.reserved_disk_bytes = disk_reserved;
                    state.stats.reserved_disk_entries = entries_reserved;
                    state.stats.disk_bytes = disk;
                    state.stats.disk_entries = state.entries.len();
                } else {
                    state
                        .entries
                        .get_mut(&job.reference)
                        .ok_or(CacheError::Poisoned)?
                        .resident = Arc::downgrade(&entry);
                }
                state.stats.resident_bytes = resident;
                entry.accounted.store(true, Ordering::Release);
                Ok(entry)
            }
        };
        state.stats.inflight_bytes = inflight;
        state.stats.pending_resident_bytes = pending;
        state.stats.running_jobs = running;
        state.jobs.remove(&job.reference);
        state.stats.inflight_jobs = state.jobs.len();
        state.epoch = state.epoch.wrapping_add(1);
        result
    })();
    if result
        .as_ref()
        .is_err_and(|error| *error == CacheError::Poisoned)
    {
        poison(shared);
    }
    if let Ok(mut output) = job.result.lock() {
        *output = Some((Instant::now(), result));
    }
    let waiters = job
        .waiters
        .lock()
        .map(|waiters| {
            waiters
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for waiter in waiters {
        waiter.notify();
    }
    shared.timer.notify_all();
}
