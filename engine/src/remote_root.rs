//! Authenticated partial-root sessions, deliberately separate from LocalStore.
//!
//! A fresh scoped replicated receipt and a current host attempt are mandatory
//! for every admission. Saved receipts/availability are reconstructible hints,
//! never restart authorization, complete closure, pins or local durability.
//! Objects are addressed only through private session-bound proof handles.
//! Each derived handle retains its verified parent snapshot; actual child
//! semantics and all aliases are checked before child bytes can be exposed.
//!
//! Request/poll/with_bytes perform bounded in-memory work only. The authority
//! callback MUST be a bounded in-memory fence/lease, never journal/network I/O.
//! Admission/open/inspection are explicitly off-actor. A single bounded worker
//! persists availability hints; a full queue drops hints, never authority.
//! Invalidating a session wakes/cancels ticket interest, not transport ownership:
//! the cache/source keep admission until real HTTP settlement or child reap.

use crate::{
    content, directory,
    protocol::{canonical_json, ObjectKind, ObjectRef, OwnerEpoch},
    remote_cache::{
        CacheError, CacheScope, CacheTicket, CachedObject, ImmutableObjectSource, RemoteCache,
    },
    remote_source::{
        NodeRemoteSource, ReplicatedDurability, ReplicatedRootReceipt, SourceError,
        SourceRequestControl,
    },
    snapshot,
};
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    future::Future,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        Arc, Mutex, Weak,
    },
    task::{Context, Poll, Waker},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, RootError>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootError {
    Busy,
    TimedOut,
    Cancelled,
    Revoked,
    ForeignHandle,
    Invalid,
    Unsupported,
    Conflict,
    Corrupt,
    SourceFailed,
    Io,
}
impl std::fmt::Display for RootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "partial-root resources are busy",
            Self::TimedOut => "partial-root request timed out",
            Self::Cancelled => "partial-root request cancelled",
            Self::Revoked => "partial-root attempt is no longer current",
            Self::ForeignHandle => "object handle belongs to another partial-root session",
            Self::Invalid => "invalid partial-root configuration or reference",
            Self::Unsupported => "object requires an explicit supported bulk/materialization path",
            Self::Conflict => "replicated root receipt differs from the expected immutable binding",
            Self::Corrupt => "partial-root object or edge failed validation",
            Self::SourceFailed => "authenticated partial-root source failed",
            Self::Io => "protected partial-root metadata I/O failed",
        })
    }
}
impl std::error::Error for RootError {}
impl RootError {
    pub fn errno(self) -> i32 {
        match self {
            Self::Busy => libc::EAGAIN,
            Self::TimedOut => libc::ETIMEDOUT,
            Self::Cancelled => libc::ECANCELED,
            Self::Revoked | Self::Conflict => libc::ESTALE,
            Self::ForeignHandle => libc::EACCES,
            Self::Invalid => libc::EINVAL,
            Self::Unsupported => libc::EOPNOTSUPP,
            _ => libc::EIO,
        }
    }
}
impl From<CacheError> for RootError {
    fn from(error: CacheError) -> Self {
        match error {
            CacheError::Busy => Self::Busy,
            CacheError::TimedOut => Self::TimedOut,
            CacheError::Cancelled | CacheError::Closed => Self::Cancelled,
            CacheError::Corrupt => Self::Corrupt,
            CacheError::InvalidReference => Self::Unsupported,
            _ => Self::SourceFailed,
        }
    }
}
impl From<SourceError> for RootError {
    fn from(error: SourceError) -> Self {
        match error {
            SourceError::Busy => Self::Busy,
            SourceError::TimedOut => Self::TimedOut,
            SourceError::Cancelled => Self::Cancelled,
            _ => Self::SourceFailed,
        }
    }
}
fn lock<T>(value: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    value.lock().map_err(|_| RootError::Io)
}
fn sql<T>(value: rusqlite::Result<T>) -> Result<T> {
    value.map_err(|error| match error.sqlite_error_code() {
        Some(
            rusqlite::ErrorCode::DiskFull
            | rusqlite::ErrorCode::DatabaseBusy
            | rusqlite::ErrorCode::DatabaseLocked,
        ) => RootError::Busy,
        _ => RootError::Io,
    })
}
fn count(db: &Connection, query: &str) -> Result<u64> {
    let value: i64 = sql(db.query_row(query, [], |row| row.get(0)))?;
    u64::try_from(value).map_err(|_| RootError::Corrupt)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptBinding {
    pub project_id: String,
    pub run_id: String,
    pub run_epoch: u64,
    pub owner: OwnerEpoch,
    pub execution_id: String,
    pub operation_id: String,
    pub view_id: String,
}
impl AttemptBinding {
    fn validate(&self) -> Result<()> {
        for value in [&self.project_id, &self.run_id, &self.owner.owner] {
            if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                return Err(RootError::Invalid);
            }
        }
        for value in [&self.execution_id, &self.operation_id, &self.view_id] {
            if !Uuid::parse_str(value).is_ok_and(|id| id.to_string() == *value) {
                return Err(RootError::Invalid);
            }
        }
        if self.run_epoch == 0 || self.owner.epoch == 0 {
            return Err(RootError::Invalid);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootBinding {
    pub scope: CacheScope,
    pub root_id: String,
    pub root: ObjectRef,
}
/// Trusted host authority only. validate MUST be bounded in-memory work, and
/// checks both the attempt and the exact intended provider/account/root tuple.
/// The host calls session.invalidate() on fencing for prompt Future wakeups.
pub trait CurrentAttemptAuthority: Send + Sync + 'static {
    fn validate(&self, attempt: &AttemptBinding, root: &RootBinding) -> anyhow::Result<()>;
}
pub trait RootReceiptSource: Send + Sync + 'static {
    fn scope(&self) -> CacheScope;
    fn get_root(
        &self,
        root_id: &str,
        control: &SourceRequestControl,
    ) -> std::result::Result<ReplicatedRootReceipt, SourceError>;
}
impl RootReceiptSource for NodeRemoteSource {
    fn scope(&self) -> CacheScope {
        ImmutableObjectSource::scope(self)
    }
    fn get_root(
        &self,
        root_id: &str,
        control: &SourceRequestControl,
    ) -> std::result::Result<ReplicatedRootReceipt, SourceError> {
        NodeRemoteSource::get_root(self, root_id, control)
    }
}
#[derive(Debug, Clone)]
pub struct RootLimits {
    pub max_sessions: usize,
    pub max_handles: usize,
    pub max_loaded_objects: usize,
    pub max_pending_loads: usize,
    pub max_borrowed_bytes: u64,
    pub max_proof_bytes: u64,
    pub max_depth: usize,
    pub max_saved_roots: usize,
    pub max_availability_hints: usize,
    pub metadata_queue: usize,
    /// Main SQLite bytes. Budget a second DB-sized DELETE rollback journal
    /// plus bounded page/header overhead; no WAL or external authority data.
    pub metadata_disk_bytes: u64,
}
impl Default for RootLimits {
    fn default() -> Self {
        Self {
            max_sessions: 32,
            max_handles: 2048,
            max_loaded_objects: 1024,
            max_pending_loads: 64,
            max_borrowed_bytes: 16 * 1024 * 1024,
            max_proof_bytes: 4 * 1024 * 1024,
            max_depth: 1024,
            max_saved_roots: 1024,
            max_availability_hints: 4096,
            metadata_queue: 64,
            metadata_disk_bytes: 16 * 1024 * 1024,
        }
    }
}
impl RootLimits {
    fn validate(&self) -> Result<()> {
        if self.max_sessions == 0
            || self.max_sessions > 1024
            || self.max_handles == 0
            || self.max_handles > 65536
            || self.max_loaded_objects == 0
            || self.max_loaded_objects > 65536
            || self.max_pending_loads == 0
            || self.max_pending_loads > 4096
            || self.max_borrowed_bytes == 0
            || self.max_borrowed_bytes > 1024 * 1024 * 1024
            || self.max_proof_bytes == 0
            || self.max_proof_bytes > 64 * 1024 * 1024
            || self.max_depth == 0
            || self.max_depth > 65536
            || self.max_saved_roots == 0
            || self.max_saved_roots > 16384
            || self.max_availability_hints == 0
            || self.max_availability_hints > 65536
            || self.metadata_queue == 0
            || self.metadata_queue > 4096
            || self.metadata_disk_bytes < 65536
            || self.metadata_disk_bytes > 64 * 1024 * 1024
        {
            Err(RootError::Invalid)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RootStats {
    pub sessions: usize,
    pub handles: usize,
    pub loaded_objects: usize,
    pub pending_loads: usize,
    pub borrowed_bytes: u64,
    pub saved_roots: u64,
    pub availability_hints: u64,
    pub dropped_hints: u64,
    pub hint_write_failures: u64,
}
struct Quotas {
    sessions: usize,
    handles: usize,
    loaded: usize,
    loads: usize,
    borrowed: u64,
    next_load: u64,
    wake: BTreeMap<u64, (Uuid, Weak<WakeSignal>)>,
}
struct Shared {
    scope: CacheScope,
    cache: RemoteCache,
    source: Arc<dyn RootReceiptSource>,
    authority: Arc<dyn CurrentAttemptAuthority>,
    limits: RootLimits,
    quota: Mutex<Quotas>,
    database: Mutex<Connection>,
    hints: SyncSender<Hint>,
    closed: AtomicBool,
    saved: AtomicU64,
    available: AtomicU64,
    dropped: AtomicU64,
    failures: AtomicU64,
    root: PathBuf,
    _lock: File,
}
struct Manager {
    shared: Arc<Shared>,
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
    }
}
#[derive(Clone)]
pub struct RemoteRootRegistry(Arc<Manager>);
enum Charge {
    Session,
    Handle,
    Load(u64),
    Loaded(u64),
}
struct Lease {
    shared: Arc<Shared>,
    charge: Charge,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.quota.lock() {
            match self.charge {
                Charge::Session => state.sessions -= 1,
                Charge::Handle => state.handles -= 1,
                Charge::Load(id) => {
                    state.loads -= 1;
                    state.wake.remove(&id);
                }
                Charge::Loaded(bytes) => {
                    state.loaded -= 1;
                    state.borrowed -= bytes;
                }
            }
        }
    }
}
struct Hint {
    root_id: String,
    reference: ObjectRef,
}
struct SessionInner {
    manager: Arc<Manager>,
    attempt: AttemptBinding,
    binding: RootBinding,
    receipt: ReplicatedRootReceipt,
    nonce: Uuid,
    invalid: AtomicBool,
    _lease: Lease,
}
#[derive(Clone)]
pub struct RemoteSession(Arc<SessionInner>);
struct Proof {
    session: Arc<SessionInner>,
    reference: ObjectRef,
    depth: usize,
    bytes: u64,
    parent: Option<Arc<Loaded>>,
    edge: usize,
    _lease: Lease,
}
#[derive(Clone)]
pub struct ObjectHandle(Arc<Proof>);

/// Replayable reference path, never an authorization token. A fresh session
/// must verify every loaded parent and bind the final exact typed reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteObjectPath {
    pub reference: ObjectRef,
    pub edges: Vec<u32>,
}
struct Loaded {
    proof: Arc<Proof>,
    data: CachedObject,
    _lease: Lease,
}
#[derive(Clone)]
pub struct LoadedObject(Arc<Loaded>);
struct WakeSignal {
    waker: Mutex<Option<Waker>>,
    cache: Mutex<Option<CacheTicket>>,
}
impl WakeSignal {
    fn cancel_cache(&self) {
        if let Ok(cache) = self.cache.lock() {
            if let Some(cache) = cache.as_ref() {
                cache.cancel();
            }
        }
    }
}
pub struct LoadTicket {
    handle: ObjectHandle,
    signal: Arc<WakeSignal>,
    result: Option<Result<LoadedObject>>,
    cancelled: bool,
    _lease: Lease,
}
impl std::fmt::Debug for ObjectHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectHandle")
            .field("reference", &self.0.reference)
            .field("depth", &self.0.depth)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for LoadedObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedObject")
            .field("reference", &self.0.proof.reference)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for RemoteSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSession")
            .field("binding", &self.0.binding)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for LoadTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadTicket")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn charge(self: &Arc<Self>, charge: Charge) -> Result<Lease> {
        let mut state = lock(&self.quota)?;
        match charge {
            Charge::Session => {
                if state.sessions >= self.limits.max_sessions {
                    return Err(RootError::Busy);
                }
                state.sessions += 1;
            }
            Charge::Handle => {
                if state.handles >= self.limits.max_handles {
                    return Err(RootError::Busy);
                }
                state.handles += 1;
            }
            Charge::Loaded(bytes) => {
                let sum = state.borrowed.checked_add(bytes).ok_or(RootError::Busy)?;
                if state.loaded >= self.limits.max_loaded_objects
                    || sum > self.limits.max_borrowed_bytes
                {
                    return Err(RootError::Busy);
                }
                state.loaded += 1;
                state.borrowed = sum;
            }
            Charge::Load(_) => return Err(RootError::Invalid),
        }
        Ok(Lease {
            shared: self.clone(),
            charge,
        })
    }
    fn load_lease(self: &Arc<Self>, session: Uuid, signal: &Arc<WakeSignal>) -> Result<Lease> {
        let mut state = lock(&self.quota)?;
        if state.loads >= self.limits.max_pending_loads {
            return Err(RootError::Busy);
        }
        let id = state.next_load;
        state.next_load = id.checked_add(1).ok_or(RootError::Busy)?;
        state.loads += 1;
        state.wake.insert(id, (session, Arc::downgrade(signal)));
        Ok(Lease {
            shared: self.clone(),
            charge: Charge::Load(id),
        })
    }
}
fn root_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        Err(RootError::Invalid)
    } else {
        Ok(())
    }
}
fn supported(reference: &ObjectRef) -> Result<()> {
    reference.validate().map_err(|_| RootError::Invalid)?;
    let limit = match reference.kind {
        ObjectKind::Blob => 4 * 1024 * 1024,
        ObjectKind::File
        | ObjectKind::Directory
        | ObjectKind::DirectoryShard
        | ObjectKind::SourceRoot
        | ObjectKind::EnvironmentRoot
        | ObjectKind::RunnableRoot => 64 * 1024,
        _ => return Err(RootError::Unsupported),
    };
    if reference.size_bytes > limit {
        Err(RootError::Unsupported)
    } else {
        Ok(())
    }
}
fn receipt_valid(receipt: &ReplicatedRootReceipt, binding: &RootBinding) -> Result<()> {
    if receipt.format_version != 1
        || receipt.root_id != binding.root_id
        || receipt.root != binding.root
        || receipt.durability != ReplicatedDurability::Replicated
        || receipt.published_at_ms > 9_007_199_254_740_991
        || receipt.object_count == 0
        || receipt.object_count > 100000
        || receipt.total_bytes > 4 * 1024 * 1024 * 1024
    {
        return Err(RootError::Conflict);
    }
    supported(&receipt.root)
}
fn object_refs(reference: &ObjectRef, bytes: &[u8]) -> Result<Vec<ObjectRef>> {
    supported(reference)?;
    if matches!(
        reference.kind,
        ObjectKind::Directory | ObjectKind::SourceRoot | ObjectKind::EnvironmentRoot
    ) {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| RootError::Corrupt)?;
        if value
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            != Some(2)
        {
            return Err(RootError::Unsupported);
        }
    }
    let result = match reference.kind {
        ObjectKind::Blob => Ok(Vec::new()),
        ObjectKind::Directory | ObjectKind::DirectoryShard => {
            directory::references_from_bytes(reference.kind, bytes)
        }
        ObjectKind::File => content::file_node_references(bytes),
        ObjectKind::SourceRoot | ObjectKind::EnvironmentRoot | ObjectKind::RunnableRoot => {
            snapshot::references(reference.kind, bytes)
        }
        _ => return Err(RootError::Unsupported),
    };
    result.map_err(|_| RootError::Corrupt)
}
fn validate_edge(parent: &Loaded, child: &ObjectRef, bytes: &[u8], edge: usize) -> Result<()> {
    let parent_ref = &parent.proof.reference;
    let parent_bytes = parent.data.bytes();
    let references = object_refs(parent_ref, parent_bytes)?;
    if references.get(edge) != Some(child) {
        return Err(RootError::ForeignHandle);
    }
    let valid = match parent_ref.kind {
        ObjectKind::Directory | ObjectKind::DirectoryShard => {
            directory::validate_child(parent_ref.kind, parent_bytes, child, bytes)
        }
        ObjectKind::File => {
            let node = content::decode_file_node(parent_bytes).map_err(|_| RootError::Corrupt)?;
            if child.kind == ObjectKind::File {
                let sub = content::decode_file_node(bytes).map_err(|_| RootError::Corrupt)?;
                for (index, reference) in node.children.iter().enumerate() {
                    if reference == child {
                        content::validate_file_child(&node, index, &sub)
                            .map_err(|_| RootError::Corrupt)?;
                    }
                }
            }
            Ok(())
        }
        ObjectKind::RunnableRoot => {
            let root = snapshot::decode_runnable(parent_bytes).map_err(|_| RootError::Corrupt)?;
            if child == &root.source || root.environment.as_ref() == Some(child) {
                snapshot::validate_child(parent_bytes, child, bytes)
            } else {
                Ok(())
            }
        }
        ObjectKind::SourceRoot | ObjectKind::EnvironmentRoot => Ok(()),
        _ => return Err(RootError::Corrupt),
    };
    valid.map_err(|_| RootError::Corrupt)
}

impl RemoteRootRegistry {
    /// Blocking startup/SQLite recovery; call on a bounded host I/O worker.
    /// base is an existing private engine-owned directory outside cache/CAS.
    pub fn open(
        base: impl AsRef<Path>,
        cache: RemoteCache,
        source: Arc<dyn RootReceiptSource>,
        authority: Arc<dyn CurrentAttemptAuthority>,
        limits: RootLimits,
    ) -> Result<Self> {
        limits.validate()?;
        let scope = source.scope();
        scope.digest().map_err(|_| RootError::Invalid)?;
        if &scope != cache.scope() {
            return Err(RootError::Conflict);
        }
        private_directory(base.as_ref())?;
        let base = base.as_ref().canonicalize().map_err(|_| RootError::Io)?;
        let predicted = base.join(format!(
            "rudder-remote-roots-v1-{}",
            scope.digest().map_err(|_| RootError::Invalid)?
        ));
        if predicted.starts_with(cache.namespace_root())
            || cache.namespace_root().starts_with(&predicted)
        {
            return Err(RootError::Invalid);
        }
        let (root, owner, database, roots, hints) = open_database(&base, &scope, &limits)?;
        let (tx, rx) = mpsc::sync_channel(limits.metadata_queue);
        let shared = Arc::new(Shared {
            scope,
            cache,
            source,
            authority,
            limits,
            quota: Mutex::new(Quotas {
                sessions: 0,
                handles: 0,
                loaded: 0,
                loads: 0,
                borrowed: 0,
                next_load: 1,
                wake: BTreeMap::new(),
            }),
            database: Mutex::new(database),
            hints: tx,
            closed: AtomicBool::new(false),
            saved: AtomicU64::new(roots),
            available: AtomicU64::new(hints),
            dropped: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            root,
            _lock: owner,
        });
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("rudder-root-hints".into())
            .spawn(move || hint_worker(worker, rx))
            .map_err(|_| RootError::Io)?;
        Ok(Self(Arc::new(Manager { shared })))
    }
    pub fn scope(&self) -> &CacheScope {
        &self.0.shared.scope
    }
    pub fn metadata_root(&self) -> &Path {
        &self.0.shared.root
    }
    pub fn stats(&self) -> Result<RootStats> {
        let shared = &self.0.shared;
        let q = lock(&shared.quota)?;
        Ok(RootStats {
            sessions: q.sessions,
            handles: q.handles,
            loaded_objects: q.loaded,
            pending_loads: q.loads,
            borrowed_bytes: q.borrowed,
            saved_roots: shared.saved.load(Ordering::Acquire),
            availability_hints: shared.available.load(Ordering::Acquire),
            dropped_hints: shared.dropped.load(Ordering::Acquire),
            hint_write_failures: shared.failures.load(Ordering::Acquire),
        })
    }
    /// Blocking fresh authenticated receipt lookup + local hint commit. No
    /// metadata/blob tree fetch, eager walk, LocalStore mutation or local pin.
    pub fn admit(
        &self,
        attempt: &AttemptBinding,
        root: &str,
        expected: &ObjectRef,
        control: &SourceRequestControl,
    ) -> Result<RemoteSession> {
        attempt.validate()?;
        root_id(root)?;
        supported(expected)?;
        if expected.kind != ObjectKind::RunnableRoot {
            return Err(RootError::Unsupported);
        }
        let shared = &self.0.shared;
        let lease = shared.charge(Charge::Session)?;
        let binding = RootBinding {
            scope: shared.scope.clone(),
            root_id: root.into(),
            root: expected.clone(),
        };
        shared
            .authority
            .validate(attempt, &binding)
            .map_err(|_| RootError::Revoked)?;
        if shared.source.scope() != binding.scope {
            return Err(RootError::Conflict);
        }
        let receipt = shared.source.get_root(root, control)?;
        receipt_valid(&receipt, &binding)?;
        if shared.source.scope() != binding.scope {
            return Err(RootError::Conflict);
        }
        shared
            .authority
            .validate(attempt, &binding)
            .map_err(|_| RootError::Revoked)?;
        save_receipt(shared, &receipt)?;
        shared
            .authority
            .validate(attempt, &binding)
            .map_err(|_| RootError::Revoked)?;
        Ok(RemoteSession(Arc::new(SessionInner {
            manager: self.0.clone(),
            attempt: attempt.clone(),
            binding,
            receipt,
            nonce: Uuid::new_v4(),
            invalid: AtomicBool::new(false),
            _lease: lease,
        })))
    }
    /// Blocking inspection only. These records NEVER mint session handles.
    pub fn saved_receipt(&self, root: &str) -> Result<Option<ReplicatedRootReceipt>> {
        root_id(root)?;
        let db = lock(&self.0.shared.database)?;
        let bytes: Option<Vec<u8>> = sql(db
            .query_row(
                "SELECT receipt FROM receipts WHERE root_id=?1",
                [root],
                |row| row.get(0),
            )
            .optional())?;
        bytes
            .map(|bytes| decode_receipt(&bytes, &self.0.shared.scope, root))
            .transpose()
    }
}
impl SessionInner {
    fn check(&self) -> Result<()> {
        if self.invalid.load(Ordering::Acquire) {
            return Err(RootError::Revoked);
        }
        if self
            .manager
            .shared
            .authority
            .validate(&self.attempt, &self.binding)
            .is_err()
        {
            self.invalid.store(true, Ordering::Release);
            return Err(RootError::Revoked);
        }
        Ok(())
    }
}
impl RemoteSession {
    /// Blocking bounded metadata traversal; call outside the engine/App actor.
    /// Saved paths cannot restore the retired session's access or expose bytes.
    pub fn resolve_path(&self, path: &RemoteObjectPath, deadline: Instant) -> Result<ObjectHandle> {
        self.0.check()?;
        supported(&path.reference)?;
        if path.edges.len() > self.0.manager.shared.limits.max_depth {
            return Err(RootError::Busy);
        }
        let mut current = self.root()?;
        for edge in &path.edges {
            if Instant::now() >= deadline {
                return Err(RootError::TimedOut);
            }
            current = self
                .request(&current, deadline)?
                .wait()?
                .child_at(*edge as usize)?;
        }
        self.validate_handle(&current)?;
        if current.reference() != &path.reference {
            return Err(RootError::ForeignHandle);
        }
        Ok(current)
    }
    /// Bounded host fence check, without I/O or new resource admission.
    pub fn check_current(&self) -> Result<()> {
        self.0.check()
    }
    /// Proves reference membership only. Child bytes and semantic edges still
    /// require request/load; this creates no local-availability or pin claim.
    pub fn validate_handle(&self, handle: &ObjectHandle) -> Result<()> {
        if !Arc::ptr_eq(&self.0, &handle.0.session) {
            return Err(RootError::ForeignHandle);
        }
        self.0.check()?;
        supported(&handle.0.reference)
    }
    pub fn attempt(&self) -> &AttemptBinding {
        &self.0.attempt
    }
    pub fn binding(&self) -> &RootBinding {
        &self.0.binding
    }
    pub fn receipt(&self) -> &ReplicatedRootReceipt {
        &self.0.receipt
    }
    pub fn invalidate(&self) {
        self.0.invalid.store(true, Ordering::Release);
        let signals = if let Ok(q) = self.0.manager.shared.quota.lock() {
            q.wake
                .values()
                .filter(|(session, _)| *session == self.0.nonce)
                .filter_map(|(_, weak)| weak.upgrade())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for signal in signals {
            signal.cancel_cache();
            if let Ok(mut waker) = signal.waker.lock() {
                if let Some(waker) = waker.take() {
                    waker.wake();
                }
            }
        }
    }
    pub fn root(&self) -> Result<ObjectHandle> {
        self.0.check()?;
        let lease = self.0.manager.shared.charge(Charge::Handle)?;
        Ok(ObjectHandle(Arc::new(Proof {
            session: self.0.clone(),
            reference: self.0.binding.root.clone(),
            depth: 0,
            bytes: 0,
            parent: None,
            edge: 0,
            _lease: lease,
        })))
    }
    /// Nonblocking admission. The cache may schedule I/O on its fixed workers.
    pub fn request(&self, handle: &ObjectHandle, deadline: Instant) -> Result<LoadTicket> {
        if !Arc::ptr_eq(&self.0, &handle.0.session) {
            return Err(RootError::ForeignHandle);
        }
        self.0.check()?;
        supported(&handle.0.reference)?;
        let signal = Arc::new(WakeSignal {
            waker: Mutex::new(None),
            cache: Mutex::new(None),
        });
        let lease = self.0.manager.shared.load_lease(self.0.nonce, &signal)?;
        let ticket = self
            .0
            .manager
            .shared
            .cache
            .request(&handle.0.reference, deadline)?;
        *lock(&signal.cache)? = Some(ticket);
        self.0.check()?;
        Ok(LoadTicket {
            handle: handle.clone(),
            signal,
            result: None,
            cancelled: false,
            _lease: lease,
        })
    }
}
impl ObjectHandle {
    pub fn reference_path(&self) -> Result<RemoteObjectPath> {
        self.0.session.check()?;
        let mut edges = Vec::with_capacity(self.0.depth);
        let mut proof = &*self.0;
        while let Some(parent) = &proof.parent {
            edges.push(proof.edge.try_into().map_err(|_| RootError::Invalid)?);
            proof = &parent.proof;
        }
        edges.reverse();
        if edges.len() != self.0.depth {
            return Err(RootError::Corrupt);
        }
        Ok(RemoteObjectPath {
            reference: self.0.reference.clone(),
            edges,
        })
    }
    pub fn reference(&self) -> &ObjectRef {
        &self.0.reference
    }
    pub fn depth(&self) -> usize {
        self.0.depth
    }
}
impl LoadedObject {
    pub fn reference(&self) -> &ObjectRef {
        &self.0.proof.reference
    }
    /// No raw CachedObject/FD escapes. Returned copies are already exposed
    /// bytes; revocation cannot retroactively erase a caller's prior copies.
    pub fn with_bytes<R>(&self, operation: impl FnOnce(&[u8]) -> R) -> Result<R> {
        self.0.proof.session.check()?;
        Ok(operation(self.0.data.bytes()))
    }
    pub fn child_at(&self, index: usize) -> Result<ObjectHandle> {
        self.0.proof.session.check()?;
        let references = object_refs(&self.0.proof.reference, self.0.data.bytes())?;
        let reference = references.get(index).ok_or(RootError::Invalid)?.clone();
        supported(&reference)?;
        let shared = &self.0.proof.session.manager.shared;
        let depth = self.0.proof.depth.checked_add(1).ok_or(RootError::Busy)?;
        let bytes = self
            .0
            .proof
            .bytes
            .checked_add(self.0.proof.reference.size_bytes)
            .ok_or(RootError::Busy)?;
        if depth > shared.limits.max_depth || bytes > shared.limits.max_proof_bytes {
            return Err(RootError::Busy);
        }
        let lease = shared.charge(Charge::Handle)?;
        Ok(ObjectHandle(Arc::new(Proof {
            session: self.0.proof.session.clone(),
            reference,
            depth,
            bytes,
            parent: Some(self.0.clone()),
            edge: index,
            _lease: lease,
        })))
    }
    pub fn child(&self, reference: &ObjectRef) -> Result<ObjectHandle> {
        self.0.proof.session.check()?;
        let references = object_refs(&self.0.proof.reference, self.0.data.bytes())?;
        let index = references
            .iter()
            .position(|item| item == reference)
            .ok_or(RootError::ForeignHandle)?;
        self.child_at(index)
    }
}
impl LoadTicket {
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.signal.cancel_cache();
        if let Ok(mut waker) = self.signal.waker.lock() {
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }
    }
    fn accept(&self, data: CachedObject) -> Result<LoadedObject> {
        self.handle.0.session.check()?;
        let reference = &self.handle.0.reference;
        object_refs(reference, data.bytes())?;
        if data.reference() != reference {
            return Err(RootError::Corrupt);
        }
        if let Some(parent) = &self.handle.0.parent {
            validate_edge(parent, reference, data.bytes(), self.handle.0.edge)?;
        }
        let shared = &self.handle.0.session.manager.shared;
        let lease = shared.charge(Charge::Loaded(reference.size_bytes.max(1)))?;
        self.handle.0.session.check()?;
        if shared
            .hints
            .try_send(Hint {
                root_id: self.handle.0.session.binding.root_id.clone(),
                reference: reference.clone(),
            })
            .is_err()
        {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(LoadedObject(Arc::new(Loaded {
            proof: self.handle.0.clone(),
            data,
            _lease: lease,
        })))
    }
    pub fn try_result(&mut self) -> Option<Result<LoadedObject>> {
        // A repeated successful poll/result must recheck revocation too.
        let status = if self.cancelled {
            Err(RootError::Cancelled)
        } else {
            self.handle.0.session.check()
        };
        if let Err(error) = status {
            self.signal.cancel_cache();
            self.result = Some(Err(error));
        }
        if let Some(result) = &self.result {
            return Some(result.clone());
        }
        let result = match lock(&self.signal.cache) {
            Ok(cache) => cache.as_ref().and_then(CacheTicket::try_result),
            Err(error) => Some(Err(if error == RootError::Busy {
                CacheError::Busy
            } else {
                CacheError::Io
            })),
        }?;
        let result = result
            .map_err(RootError::from)
            .and_then(|data| self.accept(data));
        self.result = Some(result.clone());
        Some(result)
    }
    /// Blocking convenience on a dedicated bounded host I/O worker, not App.
    pub fn wait(mut self) -> Result<LoadedObject> {
        loop {
            if let Some(result) = self.try_result() {
                return result;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Future for LoadTicket {
    type Output = Result<LoadedObject>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Ok(mut waker) = self.signal.waker.lock() {
            *waker = Some(cx.waker().clone());
        } else {
            return Poll::Ready(Err(RootError::Io));
        }
        if let Some(result) = self.try_result() {
            return Poll::Ready(result);
        }
        let polled = {
            let mut cache = match lock(&self.signal.cache) {
                Ok(cache) => cache,
                Err(error) => return Poll::Ready(Err(error)),
            };
            match cache.as_mut() {
                Some(cache) => Pin::new(cache).poll(cx),
                None => return Poll::Ready(Err(RootError::Io)),
            }
        };
        match polled {
            Poll::Pending => {
                if let Some(result) = self.try_result() {
                    Poll::Ready(result)
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(result) => {
                let result = result
                    .map_err(RootError::from)
                    .and_then(|data| self.accept(data));
                self.result = Some(result.clone());
                Poll::Ready(result)
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    format_version: u32,
    scope: CacheScope,
    meaning: String,
}
fn private_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|_| RootError::Io)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(RootError::Invalid);
    }
    Ok(())
}
fn private_file(path: &Path, max: u64) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|_| RootError::Io)?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.len() > max
    {
        return Err(RootError::Invalid);
    }
    Ok(())
}
fn create_private(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| RootError::Io)
}
fn decode_receipt(bytes: &[u8], scope: &CacheScope, root: &str) -> Result<ReplicatedRootReceipt> {
    if bytes.len() > 2048 {
        return Err(RootError::Corrupt);
    }
    let receipt: ReplicatedRootReceipt =
        serde_json::from_slice(bytes).map_err(|_| RootError::Corrupt)?;
    if canonical_json(&receipt).map_err(|_| RootError::Corrupt)? != bytes {
        return Err(RootError::Corrupt);
    }
    let binding = RootBinding {
        scope: scope.clone(),
        root_id: root.into(),
        root: receipt.root.clone(),
    };
    root_id(root)?;
    receipt_valid(&receipt, &binding)?;
    if receipt.root.kind != ObjectKind::RunnableRoot {
        return Err(RootError::Unsupported);
    }
    Ok(receipt)
}
fn open_database(
    base: &Path,
    scope: &CacheScope,
    limits: &RootLimits,
) -> Result<(PathBuf, File, Connection, u64, u64)> {
    private_directory(base)?;
    let base = base.canonicalize().map_err(|_| RootError::Io)?;
    let root = base.join(format!(
        "rudder-remote-roots-v1-{}",
        scope.digest().map_err(|_| RootError::Invalid)?
    ));
    let created = match fs::create_dir(&root) {
        Ok(()) => {
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .map_err(|_| RootError::Io)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => return Err(RootError::Io),
    };
    private_directory(&root)?;
    let lock_path = root.join("registry.lock");
    let owner = if created {
        create_private(&lock_path)?
    } else {
        private_file(&lock_path, 0)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)
            .map_err(|_| RootError::Io)?
    };
    owner.try_lock_exclusive().map_err(|_| RootError::Busy)?;
    let marker = canonical_json(&Marker {
        format_version: 1,
        scope: scope.clone(),
        meaning: "reconstructible_partial_hints_only".into(),
    })
    .map_err(|_| RootError::Invalid)?;
    if created {
        let mut file = create_private(&root.join("scope.json"))?;
        file.write_all(&marker).map_err(|_| RootError::Io)?;
        file.sync_all().map_err(|_| RootError::Io)?;
        File::open(&root)
            .and_then(|f| f.sync_all())
            .map_err(|_| RootError::Io)?;
        File::open(&base)
            .and_then(|f| f.sync_all())
            .map_err(|_| RootError::Io)?;
    } else {
        private_file(&root.join("scope.json"), 1024)?;
        if fs::read(root.join("scope.json")).map_err(|_| RootError::Io)? != marker {
            return Err(RootError::Conflict);
        }
    }
    let mut seen = 0;
    for entry in fs::read_dir(&root).map_err(|_| RootError::Io)? {
        seen += 1;
        if seen > 5 {
            return Err(RootError::Invalid);
        }
        let entry = entry.map_err(|_| RootError::Io)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(RootError::Invalid)?;
        let max = match name {
            "registry.lock" => 0,
            "scope.json" => 1024,
            "registry.sqlite" => limits.metadata_disk_bytes,
            "registry.sqlite-journal" => limits.metadata_disk_bytes * 2 + 65536,
            _ => return Err(RootError::Invalid),
        };
        private_file(&entry.path(), max)?;
    }
    let path = root.join("registry.sqlite");
    if !path.try_exists().map_err(|_| RootError::Io)? {
        if !created {
            return Err(RootError::Corrupt);
        }
        let file = create_private(&path)?;
        file.sync_all().map_err(|_| RootError::Io)?;
    }
    let db = sql(Connection::open(&path))?;
    sql(db.busy_timeout(Duration::from_millis(50)))?;
    sql(db.execute_batch("PRAGMA page_size=4096"))?;
    let page_size: i64 = sql(db.query_row("PRAGMA page_size", [], |row| row.get(0)))?;
    if page_size != 4096 {
        return Err(RootError::Unsupported);
    }
    sql(db.execute_batch(&format!("PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA journal_mode=DELETE; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-256; PRAGMA max_page_count={}; CREATE TABLE IF NOT EXISTS receipts(root_id TEXT PRIMARY KEY,receipt BLOB NOT NULL CHECK(length(receipt)<=2048)); CREATE TABLE IF NOT EXISTS availability(root_id TEXT NOT NULL REFERENCES receipts(root_id),object TEXT NOT NULL CHECK(length(object)<=512),seen INTEGER NOT NULL,PRIMARY KEY(root_id,object));",limits.metadata_disk_bytes/4096)))?;
    let quick: String = sql(db.query_row("PRAGMA quick_check", [], |row| row.get(0)))?;
    if quick != "ok" {
        return Err(RootError::Corrupt);
    }
    {
        let mut statement = sql(db.prepare("PRAGMA foreign_key_check"))?;
        let mut rows = sql(statement.query([]))?;
        if sql(rows.next())?.is_some() {
            return Err(RootError::Corrupt);
        }
    }
    let roots: u64 = count(&db, "SELECT count(*) FROM receipts")?;
    let hints: u64 = count(&db, "SELECT count(*) FROM availability")?;
    if roots > limits.max_saved_roots as u64 || hints > limits.max_availability_hints as u64 {
        return Err(RootError::Busy);
    }
    {
        let mut statement = sql(db.prepare("SELECT root_id,receipt FROM receipts"))?;
        let mut rows = sql(statement.query([]))?;
        while let Some(row) = sql(rows.next())? {
            let root: String = sql(row.get(0))?;
            let bytes: Vec<u8> = sql(row.get(1))?;
            decode_receipt(&bytes, scope, &root)?;
        }
    }
    {
        let mut statement = sql(db.prepare("SELECT object FROM availability"))?;
        let mut rows = sql(statement.query([]))?;
        while let Some(row) = sql(rows.next())? {
            let bytes: String = sql(row.get(0))?;
            if bytes.len() > 512 {
                return Err(RootError::Corrupt);
            }
            let reference: ObjectRef =
                serde_json::from_str(&bytes).map_err(|_| RootError::Corrupt)?;
            supported(&reference)?;
            if canonical_json(&reference).map_err(|_| RootError::Corrupt)? != bytes.as_bytes() {
                return Err(RootError::Corrupt);
            }
        }
    }
    File::open(&root)
        .and_then(|f| f.sync_all())
        .map_err(|_| RootError::Io)?;
    Ok((root, owner, db, roots, hints))
}
fn save_receipt(shared: &Shared, receipt: &ReplicatedRootReceipt) -> Result<()> {
    let bytes = canonical_json(receipt).map_err(|_| RootError::Corrupt)?;
    if bytes.len() > 2048 {
        return Err(RootError::Corrupt);
    }
    let mut db = lock(&shared.database)?;
    let transaction = sql(db.transaction())?;
    let existing: Option<Vec<u8>> = sql(transaction
        .query_row(
            "SELECT receipt FROM receipts WHERE root_id=?1",
            [&receipt.root_id],
            |row| row.get(0),
        )
        .optional())?;
    if let Some(existing) = existing {
        if existing != bytes {
            return Err(RootError::Conflict);
        }
    } else {
        let count: u64 = count(&transaction, "SELECT count(*) FROM receipts")?;
        if count >= shared.limits.max_saved_roots as u64 {
            return Err(RootError::Busy);
        }
        sql(transaction.execute(
            "INSERT INTO receipts(root_id,receipt) VALUES(?1,?2)",
            params![receipt.root_id, bytes],
        ))?;
    }
    sql(transaction.commit())?;
    shared.saved.store(
        count(&db, "SELECT count(*) FROM receipts")?,
        Ordering::Release,
    );
    Ok(())
}
fn save_hint(shared: &Shared, hint: Hint) -> Result<()> {
    let bytes = String::from_utf8(canonical_json(&hint.reference).map_err(|_| RootError::Corrupt)?)
        .map_err(|_| RootError::Corrupt)?;
    let mut db = lock(&shared.database)?;
    let transaction = sql(db.transaction())?;
    let present: bool = sql(transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM availability WHERE root_id=?1 AND object=?2)",
        params![hint.root_id, bytes],
        |row| row.get(0),
    ))?;
    if !present {
        let count: u64 = count(&transaction, "SELECT count(*) FROM availability")?;
        if count >= shared.limits.max_availability_hints as u64 {
            sql(transaction.execute("DELETE FROM availability WHERE rowid=(SELECT rowid FROM availability ORDER BY seen,rowid LIMIT 1)",[]))?;
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    sql(transaction.execute("INSERT INTO availability(root_id,object,seen) VALUES(?1,?2,?3) ON CONFLICT(root_id,object) DO UPDATE SET seen=excluded.seen",params![hint.root_id,bytes,now]))?;
    sql(transaction.commit())?;
    shared.available.store(
        count(&db, "SELECT count(*) FROM availability")?,
        Ordering::Release,
    );
    Ok(())
}
fn hint_worker(shared: Arc<Shared>, receiver: Receiver<Hint>) {
    while !shared.closed.load(Ordering::Acquire) {
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(hint) => {
                if save_hint(&shared, hint).is_err() {
                    shared.failures.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
