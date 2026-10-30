//! Scoped observation transport for the process owning a mounted VfsView.
//! This service never reopens a view, changes its root, seals it, stops writers,
//! or grants journal/CAS access. Even matching generations are LiveBestEffort.
//! Returned roots stay pinned until explicit retirement or a trusted durable-WC
//! witness adopts their replacement. Disconnect/crash never guesses adoption.

use super::{
    DirtyCursor, DirtyPage, ObservationBudget, ObservationConsistency, VfsObservation, VfsView,
};
use crate::{
    protocol::{canonical_json, ObjectKind, ObjectRef, OwnerEpoch},
    store::{ImportPolicy, LocalStore, ReferencedMetadata},
    workspace::WorkspaceProfile,
};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const MAX_DIRTY_EVENTS: usize = 128;
const IO_TIMEOUT: Duration = Duration::from_millis(20);
const FRAME_DEADLINE: Duration = Duration::from_secs(2);
const CLIENT_DEADLINE: Duration = Duration::from_secs(35);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptContext {
    pub execution_id: String,
    pub operation_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationBinding {
    pub format_version: u32,
    pub attempt: AttemptContext,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub namespace: String,
    pub profile: ObjectRef,
    pub exclusion_policy_digest: String,
}
impl ObservationBinding {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.format_version == 1, "unsupported observation binding");
        for id in [
            &self.attempt.execution_id,
            &self.attempt.operation_id,
            &self.view_id,
            &self.namespace,
        ] {
            uuid(id)?;
        }
        self.owner.validate()?;
        ensure!(
            self.owner.owner.len() <= 512,
            "owner identity exceeds bound"
        );
        self.profile.validate()?;
        ensure!(
            self.profile.kind == ObjectKind::Blob && self.profile.size_bytes <= 4 * 1024 * 1024,
            "invalid workspace profile reference"
        );
        digest(&self.exclusion_policy_digest)?;
        Ok(())
    }
    fn id(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }
}

/// A workspace-only capability. Do not Debug-format or log its token.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationIdentity {
    pub format_version: u32,
    pub binding: ObservationBinding,
    pub token: String,
}
impl ObservationIdentity {
    fn validate(&self) -> Result<()> {
        ensure!(self.format_version == 1, "unsupported observation identity");
        self.binding.validate()?;
        digest(&self.token)
    }
}

pub trait AttemptAuthority: Send + Sync {
    fn validate(&self, binding: &ObservationBinding) -> Result<()>;
    /// A trusted live-authority monitor may invalidate in-progress cooperative
    /// work without performing an authority RPC for every file/block. A false
    /// value is not authorization; dispatch/result still call validate.
    fn cancelled(&self) -> bool {
        false
    }
}

/// Evidence is a lookup key into configured private durable state, never proof
/// by itself. The callback must hold its trusted publication guard through the
/// adoption transaction. It must not reacquire a WC lock held by the RPC caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptionEvidence {
    pub state_sha256: String,
    pub tree: serde_json::Value,
}
pub trait AdoptionGuard {}
impl<T> AdoptionGuard for T {}
pub trait AdoptionWitness: Send + Sync {
    fn verify<'a>(
        &'a self,
        binding: &ObservationBinding,
        observation: &VfsObservation,
        evidence: &AdoptionEvidence,
    ) -> Result<Box<dyn AdoptionGuard + 'a>>;
}

#[derive(Debug, Clone)]
pub struct RpcLimits {
    pub max_connections: usize,
    pub max_calls: usize,
    pub max_live_observations: usize,
    pub max_receipts: usize,
    pub observation: ObservationBudget,
}
impl Default for RpcLimits {
    fn default() -> Self {
        Self {
            max_connections: 16,
            max_calls: 2,
            max_live_observations: 256,
            max_receipts: 4096,
            observation: ObservationBudget::default(),
        }
    }
}
impl RpcLimits {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=32).contains(&self.max_connections) && (1..=4).contains(&self.max_calls),
            "invalid RPC concurrency limits"
        );
        ensure!(
            (1..=256).contains(&self.max_live_observations)
                && (1..=4096).contains(&self.max_receipts),
            "invalid observation retention limits"
        );
        ensure!(
            self.observation.max_elapsed_ms > 0
                && self.observation.max_elapsed_ms <= 30_000
                && self.observation.max_changed_entries <= 100_000
                && self.observation.max_content_bytes <= 256 * 1024 * 1024
                && self.observation.max_metadata_bytes <= 128 * 1024 * 1024,
            "observation limits exceed qualified bounds"
        );
        Ok(())
    }
}
pub struct RpcConfig {
    pub attempt: AttemptContext,
    pub profile: ObjectRef,
    pub authority: Arc<dyn AttemptAuthority>,
    pub adoption_witness: Option<Arc<dyn AdoptionWitness>>,
    pub limits: RpcLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Cursor,
    Dirty {
        after: DirtyCursor,
        limit: usize,
    },
    Observe {
        nonce: String,
    },
    Inspect {
        nonce: String,
    },
    Release {
        nonce: String,
    },
    Adopt {
        operation_id: String,
        expected_current: Option<String>,
        nonce: String,
        evidence: AdoptionEvidence,
    },
    Head,
    Cancel {
        request_id: String,
    },
}
impl<'de> Deserialize<'de> for Operation {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // Serde's internally tagged unit variants otherwise accept extra fields,
        // even with deny_unknown_fields on the enum. Keep the wire surface exact.
        struct Fields;
        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = serde_json::Map<String, serde_json::Value>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("an operation object with distinct fields")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut fields = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if fields.contains_key(&key) {
                        return Err(serde::de::Error::custom(
                            "duplicate workspace operation field",
                        ));
                    }
                    fields.insert(key, map.next_value()?);
                }
                Ok(fields)
            }
        }
        let fields = deserializer.deserialize_map(Fields)?;
        let operation = fields
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| serde::de::Error::custom("operation is required"))?;
        let allowed: &[&str] = match operation {
            "cursor" | "head" => &["operation"],
            "dirty" => &["operation", "after", "limit"],
            "observe" | "inspect" | "release" => &["operation", "nonce"],
            "adopt" => &[
                "operation",
                "operation_id",
                "expected_current",
                "nonce",
                "evidence",
            ],
            "cancel" => &["operation", "request_id"],
            _ => return Err(serde::de::Error::custom("unsupported workspace operation")),
        };
        if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(serde::de::Error::custom(
                "unknown workspace operation field",
            ));
        }
        let required = |name: &str| {
            fields
                .get(name)
                .cloned()
                .ok_or_else(|| serde::de::Error::custom(format!("missing {name}")))
        };
        let decode = |name: &str| -> std::result::Result<String, D::Error> {
            serde_json::from_value(required(name)?).map_err(serde::de::Error::custom)
        };
        Ok(match operation {
            "cursor" => Self::Cursor,
            "head" => Self::Head,
            "dirty" => Self::Dirty {
                after: serde_json::from_value(required("after")?)
                    .map_err(serde::de::Error::custom)?,
                limit: serde_json::from_value(required("limit")?)
                    .map_err(serde::de::Error::custom)?,
            },
            "observe" => Self::Observe {
                nonce: decode("nonce")?,
            },
            "inspect" => Self::Inspect {
                nonce: decode("nonce")?,
            },
            "release" => Self::Release {
                nonce: decode("nonce")?,
            },
            "adopt" => Self::Adopt {
                operation_id: decode("operation_id")?,
                expected_current: serde_json::from_value(
                    fields
                        .get("expected_current")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                )
                .map_err(serde::de::Error::custom)?,
                nonce: decode("nonce")?,
                evidence: serde_json::from_value(required("evidence")?)
                    .map_err(serde::de::Error::custom)?,
            },
            "cancel" => Self::Cancel {
                request_id: decode("request_id")?,
            },
            _ => unreachable!(),
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    Pending,
    Retained,
    Adopted,
    Retiring,
    Released,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationReceipt {
    pub nonce: String,
    pub state: ReceiptState,
    pub observation: Option<VfsObservation>,
    pub retained: bool,
    pub current: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptionReceipt {
    pub operation_id: String,
    pub previous: Option<String>,
    /// The head installed by this operation. A historical retry returns its
    /// original receipt; use Head to read the currently adopted observation.
    pub current: String,
    pub sequence: u64,
    pub state_sha256: String,
    pub tree_sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Reply {
    Cursor(DirtyCursor),
    Dirty(DirtyPage),
    Observation(ObservationReceipt),
    Adoption(AdoptionReceipt),
    Head(Option<AdoptionReceipt>),
    Cancellation { request_found: bool },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    format_version: u32,
    id: String,
    identity: ObservationIdentity,
    operation: Operation,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    format_version: u32,
    id: String,
    binding: String,
    result: Option<Reply>,
    error: Option<RpcError>,
}

struct EndpointLock(File);
impl Drop for EndpointLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
struct CounterGuard {
    counter: Arc<AtomicUsize>,
}
impl CounterGuard {
    fn acquire(counter: &Arc<AtomicUsize>, maximum: usize) -> Result<Self> {
        let mut value = counter.load(Ordering::Acquire);
        loop {
            if value >= maximum {
                return Err(problem("busy", "workspace observation RPC is at capacity"));
            }
            match counter.compare_exchange_weak(
                value,
                value + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(Self {
                        counter: counter.clone(),
                    })
                }
                Err(current) => value = current,
            }
        }
    }
}
impl Drop for CounterGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Inner {
    core: Core,
    identity: ObservationIdentity,
    socket: PathBuf,
    socket_inode: (u64, u64),
    stopped: AtomicBool,
    accept_done: AtomicBool,
    connections: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    streams: Mutex<BTreeMap<String, UnixStream>>,
    requests: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
    endpoint_locks: Mutex<Option<(EndpointLock, EndpointLock)>>,
}
impl Inner {
    fn close(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Ok(streams) = self.streams.lock() {
            for stream in streams.values() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
    fn authenticate(&self, identity: &ObservationIdentity) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(problem(
                "unavailable",
                "workspace observation endpoint is closing",
            ));
        }
        identity.validate().map_err(|_| {
            problem(
                "unauthenticated",
                "workspace observation identity is invalid",
            )
        })?;
        let a = identity.token.as_bytes();
        let b = self.identity.token.as_bytes();
        let same = a.len() == b.len()
            && a.iter()
                .zip(b)
                .fold(0u8, |difference, (a, b)| difference | (a ^ b))
                == 0;
        if !same || identity.binding != self.core.binding {
            return Err(problem(
                "unauthenticated",
                "workspace observation capability binding is invalid",
            ));
        }
        self.core
            .authority
            .validate(&self.core.binding)
            .map_err(|_| {
                problem(
                    "stale_attempt",
                    "workspace attempt authority is no longer current",
                )
            })
    }
}

/// Owns a clone of the actual mounted view and a protected per-attempt endpoint.
/// Close revokes ingress immediately. The helper must await_closed successfully
/// before it unmounts/checkpoints; this is not a namespace/writer-drain witness.
pub struct ObservationRpc {
    inner: Arc<Inner>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcActivity {
    pub connections: usize,
    pub calls: usize,
}
impl ObservationRpc {
    pub fn bind(view: VfsView, config: RpcConfig, directory: &Path) -> Result<Self> {
        // Endpoint paths are a projection detail, not the ownership authority.
        // A second path must not create a second nonce/adoption ingress for the
        // same actual view. Keep this independent of the mounted-view lock,
        // which is shared by all clones of its owner.
        let view_lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(view.0.path.join("workspace-rpc.lock"))?;
        private_regular(&view_lock_file.metadata()?)?;
        FileExt::try_lock_exclusive(&view_lock_file)
            .context("mounted view already has an observation RPC owner")?;
        let view_lock = EndpointLock(view_lock_file);
        let core = Core::new(view, config)?;
        let parent = directory
            .parent()
            .context("RPC endpoint requires private parent")?
            .canonicalize()?;
        private_directory(&parent, false)?;
        let directory = parent.join(directory.file_name().context("invalid RPC endpoint name")?);
        private_directory(&directory, true)?;
        ensure!(
            !directory.starts_with(core.view.manager().store().root()),
            "RPC endpoint cannot be inside CAS"
        );
        let mut lock_name = directory.file_name().unwrap().to_os_string();
        lock_name.push(".lock");
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent.join(lock_name))?;
        private_regular(&lock_file.metadata()?)?;
        FileExt::try_lock_exclusive(&lock_file).context("workspace endpoint is already owned")?;
        let lock = EndpointLock(lock_file);
        let socket = directory.join("workspace.sock");
        socket_length(&socket)?;
        let identity_path = directory.join("identity.json");
        for entry in fs::read_dir(&directory)? {
            let name = entry?.file_name();
            ensure!(
                name == "identity.json" || name == "workspace.sock",
                "unexpected workspace endpoint entry"
            );
        }
        let identity = match fs::symlink_metadata(&identity_path) {
            Ok(_) => {
                let identity = read_identity(&identity_path)?;
                ensure!(
                    identity.binding == core.binding,
                    "endpoint identity belongs to another exact attempt"
                );
                identity
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let token = hex::encode(Sha256::digest(
                    [
                        Uuid::new_v4().as_bytes().as_slice(),
                        Uuid::new_v4().as_bytes().as_slice(),
                    ]
                    .concat(),
                ));
                let identity = ObservationIdentity {
                    format_version: 1,
                    binding: core.binding.clone(),
                    token,
                };
                let bytes = canonical_json(&identity)?;
                let temporary = directory.join(format!(".identity-{}.tmp", Uuid::new_v4()));
                let result = (|| {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                        .open(&temporary)?;
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                    fs::rename(&temporary, &identity_path)?;
                    File::open(&directory)?.sync_all()?;
                    Ok::<_, anyhow::Error>(())
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&temporary);
                }
                result?;
                identity
            }
            Err(error) => return Err(error.into()),
        };
        if let Ok(before) = fs::symlink_metadata(&socket) {
            use std::os::unix::fs::FileTypeExt;
            ensure!(
                before.file_type().is_socket() && before.uid() == unsafe { libc::geteuid() },
                "unexpected workspace socket type/owner"
            );
            match connect_until(&socket, Instant::now() + Duration::from_millis(250)) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) => {}
                _ => bail!("workspace endpoint still has a live or uncertain listener"),
            }
            let after = fs::symlink_metadata(&socket)?;
            ensure!(
                before.dev() == after.dev() && before.ino() == after.ino(),
                "workspace endpoint changed during reconciliation"
            );
            fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let metadata = fs::symlink_metadata(&socket)?;
        let inner = Arc::new(Inner {
            core,
            identity,
            socket,
            socket_inode: (metadata.dev(), metadata.ino()),
            stopped: AtomicBool::new(false),
            accept_done: AtomicBool::new(false),
            connections: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
            streams: Mutex::new(BTreeMap::new()),
            requests: Mutex::new(BTreeMap::new()),
            endpoint_locks: Mutex::new(Some((lock, view_lock))),
        });
        let running = inner.clone();
        thread::Builder::new()
            .name("rudder-vfs-rpc".into())
            .spawn(move || {
                while !running.stopped.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let guard = match CounterGuard::acquire(
                                &running.connections,
                                running.core.limits.max_connections,
                            ) {
                                Ok(guard) => guard,
                                Err(_) => {
                                    drop(stream);
                                    continue;
                                }
                            };
                            let worker = running.clone();
                            let _ = thread::Builder::new()
                                .name("rudder-vfs-rpc-call".into())
                                .spawn(move || {
                                    let _guard = guard;
                                    serve_connection(worker, stream);
                                });
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                if fs::symlink_metadata(&running.socket)
                    .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == running.socket_inode)
                {
                    let _ = fs::remove_file(&running.socket);
                }
                running.accept_done.store(true, Ordering::Release);
            })?;
        Ok(Self { inner })
    }
    pub fn binding(&self) -> &ObservationBinding {
        &self.inner.core.binding
    }
    pub fn activity(&self) -> RpcActivity {
        RpcActivity {
            connections: self.inner.connections.load(Ordering::Acquire),
            calls: self.inner.calls.load(Ordering::Acquire),
        }
    }
    pub fn socket_path(&self) -> &Path {
        &self.inner.socket
    }
    pub fn identity_path(&self) -> PathBuf {
        self.inner.socket.with_file_name("identity.json")
    }
    pub fn close(&self) {
        self.inner.close();
    }
    pub fn close_callback(&self) -> Arc<dyn Fn() + Send + Sync> {
        let inner = self.inner.clone();
        Arc::new(move || inner.close())
    }
    pub fn await_closed(&self, timeout: Duration) -> Result<()> {
        ensure!(
            timeout <= Duration::from_secs(35),
            "close wait exceeds qualified bound"
        );
        self.close();
        let deadline = Instant::now() + timeout;
        while !self.inner.accept_done.load(Ordering::Acquire)
            || self.inner.connections.load(Ordering::Acquire) != 0
        {
            if Instant::now() >= deadline {
                return Err(problem(
                    "deadline",
                    "workspace observation calls are still settling; do not checkpoint",
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
        // All old calls have settled and ingress is permanently stopped. Release
        // the ownership lock now, rather than relying on the accept thread's
        // last Arc being dropped at a particular scheduling boundary.
        self.inner
            .endpoint_locks
            .lock()
            .map_err(|_| problem("unavailable", "workspace endpoint lock unavailable"))?
            .take();
        Ok(())
    }
}
impl Drop for ObservationRpc {
    fn drop(&mut self) {
        self.close();
    }
}

fn serve_connection(inner: Arc<Inner>, mut stream: UnixStream) {
    // BSD accept inherits O_NONBLOCK; bounded blocking reads/writes require an
    // explicit normalization before using socket timeouts.
    if stream
        .set_nonblocking(false)
        .and_then(|_| stream.set_read_timeout(Some(IO_TIMEOUT)))
        .and_then(|_| stream.set_write_timeout(Some(IO_TIMEOUT)))
        .is_err()
    {
        return;
    }
    if !same_peer_user(&stream) {
        return;
    }
    let connection_id = Uuid::new_v4().to_string();
    if let Ok(copy) = stream.try_clone() {
        if let Ok(mut streams) = inner.streams.lock() {
            streams.insert(connection_id.clone(), copy);
        }
    }
    struct ConnectionGuard {
        inner: Arc<Inner>,
        id: String,
        request: Option<String>,
    }
    impl Drop for ConnectionGuard {
        fn drop(&mut self) {
            if let Ok(mut streams) = self.inner.streams.lock() {
                streams.remove(&self.id);
            }
            if let Some(id) = &self.request {
                if let Ok(mut requests) = self.inner.requests.lock() {
                    requests.remove(id);
                }
            }
        }
    }
    let mut cleanup = ConnectionGuard {
        inner: inner.clone(),
        id: connection_id,
        request: None,
    };
    let bytes = match read_frame(
        &mut stream,
        MAX_REQUEST_BYTES,
        Instant::now() + FRAME_DEADLINE,
        || inner.stopped.load(Ordering::Acquire),
    ) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    let request: Request = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(_) => return,
    };
    if request.format_version != 1 || uuid(&request.id).is_err() {
        return;
    }
    let result =
        (|| {
            inner.authenticate(&request.identity)?;
            if let Operation::Cancel { request_id } = &request.operation {
                uuid(request_id)?;
                let requests = inner.requests.lock().map_err(|_| {
                    problem("unavailable", "workspace request registry unavailable")
                })?;
                let found = requests.get(request_id);
                if let Some(cancelled) = found {
                    cancelled.store(true, Ordering::Release);
                }
                return Ok(Reply::Cancellation {
                    request_found: found.is_some(),
                });
            }
            let _call = CounterGuard::acquire(&inner.calls, inner.core.limits.max_calls)?;
            let cancelled = Arc::new(AtomicBool::new(false));
            {
                let mut requests = inner.requests.lock().map_err(|_| {
                    problem("unavailable", "workspace request registry unavailable")
                })?;
                ensure!(
                    !requests.contains_key(&request.id),
                    "duplicate active RPC request identifier"
                );
                requests.insert(request.id.clone(), cancelled.clone());
                cleanup.request = Some(request.id.clone());
            }
            let deadline = Instant::now()
                + Duration::from_millis(inner.core.limits.observation.max_elapsed_ms);
            let stopped = || {
                inner.stopped.load(Ordering::Acquire)
                    || cancelled.load(Ordering::Acquire)
                    || inner.core.authority.cancelled()
                    || Instant::now() >= deadline
                    || disconnected(&stream)
            };
            ensure!(!stopped(), "workspace call cancelled before effects");
            inner.core.validate()?;
            ensure!(!stopped(), "workspace call cancelled before effects");
            let reply = match &request.operation {
                Operation::Cursor => Reply::Cursor(inner.core.view.dirty_cursor()?),
                Operation::Dirty { after, limit } => {
                    ensure!(
                        *limit > 0 && *limit <= MAX_DIRTY_EVENTS,
                        "dirty page exceeds qualified bound"
                    );
                    ensure!(
                        after.view_id == inner.core.binding.view_id
                            && after.owner == inner.core.binding.owner,
                        "dirty cursor belongs to another view/owner"
                    );
                    Reply::Dirty(inner.core.view.dirty_since(after, *limit)?)
                }
                Operation::Observe { nonce } => {
                    Reply::Observation(inner.core.observe(nonce, &stopped)?)
                }
                Operation::Inspect { nonce } => Reply::Observation(inner.core.inspect(nonce)?),
                Operation::Release { nonce } => Reply::Observation(inner.core.release(nonce)?),
                Operation::Adopt {
                    operation_id,
                    expected_current,
                    nonce,
                    evidence,
                } => Reply::Adoption(inner.core.adopt(
                    operation_id,
                    expected_current,
                    nonce,
                    evidence,
                    &stopped,
                )?),
                Operation::Head => {
                    let c = inner.core.view.db()?;
                    Reply::Head(inner.core.head_locked(&c)?)
                }
                Operation::Cancel { .. } => unreachable!(),
            };
            // Cancellation after an effect is uncertain to the caller, never an
            // excuse to retire its durable pin or invent an adoption receipt.
            ensure!(
                !stopped(),
                "workspace call cancelled; inspect scoped receipts before retry"
            );
            inner.authenticate(&request.identity)?;
            Ok(reply)
        })();
    let (result, error) = match result {
        Ok(result) => (Some(result), None),
        Err(error) => (None, Some(error.downcast_ref::<RpcError>().cloned().unwrap_or_else(|| RpcError { code: "operation_failed".into(), message: "workspace operation failed; inspect its scoped receipt before retrying an uncertain request".into() }))),
    };
    let response = Response {
        format_version: 1,
        id: request.id,
        binding: inner.core.binding_id.clone(),
        result,
        error,
    };
    let _ = write_frame(
        &mut stream,
        &response,
        MAX_RESPONSE_BYTES,
        Instant::now() + FRAME_DEADLINE,
        || inner.stopped.load(Ordering::Acquire),
    );
}

#[derive(Clone)]
pub struct ObservationClient {
    socket: PathBuf,
    identity: PathBuf,
    binding: ObservationBinding,
}
impl ObservationClient {
    pub fn new(
        socket: impl Into<PathBuf>,
        identity: impl Into<PathBuf>,
        binding: ObservationBinding,
    ) -> Result<Self> {
        binding.validate()?;
        let client = Self {
            socket: socket.into(),
            identity: identity.into(),
            binding,
        };
        socket_length(&client.socket)?;
        ensure!(
            read_identity(&client.identity)?.binding == client.binding,
            "workspace identity differs from configured exact attempt"
        );
        Ok(client)
    }
    pub fn call(&self, operation: Operation) -> Result<Reply> {
        self.call_with_cancel(operation, &AtomicBool::new(false))
    }
    pub fn call_with_cancel(&self, operation: Operation, cancel: &AtomicBool) -> Result<Reply> {
        self.call_identified(&Uuid::new_v4().to_string(), operation, cancel)
    }
    pub fn call_identified(
        &self,
        id: &str,
        operation: Operation,
        cancel: &AtomicBool,
    ) -> Result<Reply> {
        uuid(id)?;
        if cancel.load(Ordering::Acquire) {
            return Err(problem(
                "cancelled",
                "workspace caller stopped before dispatch",
            ));
        }
        let identity = read_identity(&self.identity)?;
        ensure!(
            identity.binding == self.binding,
            "workspace identity belongs to another attempt"
        );
        let id = id.to_string();
        let mut stream = connect_until(&self.socket, Instant::now() + FRAME_DEADLINE)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        write_frame(
            &mut stream,
            &Request {
                format_version: 1,
                id: id.clone(),
                identity,
                operation,
            },
            MAX_REQUEST_BYTES,
            Instant::now() + FRAME_DEADLINE,
            || cancel.load(Ordering::Acquire),
        )?;
        let bytes = read_frame(
            &mut stream,
            MAX_RESPONSE_BYTES,
            Instant::now() + CLIENT_DEADLINE,
            || cancel.load(Ordering::Acquire),
        )?;
        let response: Response = serde_json::from_slice(&bytes)?;
        ensure!(
            response.format_version == 1
                && response.id == id
                && response.binding == self.binding.id()?
                && response.result.is_some() != response.error.is_some(),
            "workspace response binding is invalid"
        );
        if let Some(error) = response.error {
            return Err(error.into());
        }
        Ok(response.result.unwrap())
    }
}

pub fn read_identity(path: &Path) -> Result<ObservationIdentity> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let before = file.metadata()?;
    private_regular(&before)?;
    ensure!(
        before.len() <= MAX_REQUEST_BYTES as u64,
        "workspace identity exceeds byte bound"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() as u64 == before.len()
            && after.len() == before.len()
            && after.ino() == before.ino()
            && after.dev() == before.dev(),
        "workspace identity changed while reading"
    );
    let identity: ObservationIdentity = serde_json::from_slice(&bytes)?;
    identity.validate()?;
    Ok(identity)
}
fn private_regular(metadata: &fs::Metadata) -> Result<()> {
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o600
            && metadata.nlink() == 1,
        "workspace private file has unsafe type/owner/mode/links"
    );
    Ok(())
}
fn private_directory(path: &Path, create: bool) -> Result<()> {
    let mut created = false;
    if create {
        match fs::create_dir(path) {
            Ok(()) => created = true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    if created {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o700,
        "workspace endpoint directory must be private and owned"
    );
    Ok(())
}
fn socket_length(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path.as_os_str().as_bytes().len() < 100
            && !path.as_os_str().as_bytes().contains(&0),
        "workspace socket must use an absolute path shorter than100bytes"
    );
    Ok(())
}

fn connect_until(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() >= 100 || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid workspace socket path",
        ));
    }
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        || unsafe { libc::fcntl(raw, libc::F_SETFL, libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as _;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as u8;
    }
    if unsafe { libc::connect(raw, (&address as *const libc::sockaddr_un).cast(), length) } < 0 {
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
        ) {
            return Err(error);
        }
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace connect deadline",
                ));
            }
            let mut event = libc::pollfd {
                fd: raw,
                events: libc::POLLOUT,
                revents: 0,
            };
            let result =
                unsafe { libc::poll(&mut event, 1, remaining.as_millis().min(100).max(1) as i32) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if result > 0 {
                let mut error: libc::c_int = 0;
                let mut size = std::mem::size_of_val(&error) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        raw,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        (&mut error as *mut libc::c_int).cast(),
                        &mut size,
                    )
                } < 0
                {
                    return Err(io::Error::last_os_error());
                }
                if error != 0 {
                    return Err(io::Error::from_raw_os_error(error));
                }
                break;
            }
        }
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    Ok(stream)
}
fn same_peer_user(stream: &UnixStream) -> bool {
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
        return unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        } == 0
            && credentials.uid == unsafe { libc::geteuid() };
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut uid = 0;
        let mut gid = 0;
        (unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) }) == 0
            && uid == unsafe { libc::geteuid() }
    }
}
fn disconnected(stream: &UnixStream) -> bool {
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of_val(&pid) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut length,
            )
        };
        result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTCONN)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut event = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        (unsafe { libc::poll(&mut event, 1, 0) }) >= 0
            && event.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
    }
}
fn read_frame(
    stream: &mut UnixStream,
    limit: usize,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<u8>> {
    fn exact(
        stream: &mut UnixStream,
        bytes: &mut [u8],
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
    ) -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            if cancelled() {
                return Err(problem(
                    "cancelled",
                    "workspace caller stopped waiting; inspect any pending observation",
                ));
            }
            if Instant::now() >= deadline {
                return Err(problem("deadline", "workspace frame deadline exceeded"));
            }
            match stream.read(&mut bytes[offset..]) {
                Ok(0) => {
                    return Err(problem(
                        "unavailable",
                        "workspace connection ended without confirmation",
                    ))
                }
                Ok(count) => offset += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    return Err(problem(
                        "unavailable",
                        "workspace transport failed without confirmation",
                    ))
                }
            }
        }
        Ok(())
    }
    let mut prefix = [0u8; 4];
    exact(stream, &mut prefix, deadline, &cancelled)?;
    let length = u32::from_be_bytes(prefix) as usize;
    ensure!(
        length > 0 && length <= limit,
        "workspace frame exceeds byte bound"
    );
    let mut bytes = vec![0; length];
    exact(stream, &mut bytes, deadline, &cancelled)?;
    Ok(bytes)
}
fn write_frame(
    stream: &mut UnixStream,
    value: &impl Serialize,
    limit: usize,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= limit,
        "workspace result exceeds frame byte bound"
    );
    for part in [&(bytes.len() as u32).to_be_bytes()[..], bytes.as_slice()] {
        let mut offset = 0;
        while offset < part.len() {
            if cancelled() {
                return Err(problem("cancelled", "workspace caller stopped waiting"));
            }
            if Instant::now() >= deadline {
                return Err(problem("deadline", "workspace response deadline exceeded"));
            }
            match stream.write(&part[offset..]) {
                Ok(0) => {
                    return Err(problem(
                        "unavailable",
                        "workspace connection stopped accepting bytes",
                    ))
                }
                Ok(count) => offset += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    return Err(problem(
                        "unavailable",
                        "workspace response ended without confirmation",
                    ))
                }
            }
        }
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}
impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for RpcError {}
fn problem(code: &str, message: &str) -> anyhow::Error {
    RpcError {
        code: code.into(),
        message: message.into(),
    }
    .into()
}
fn uuid(value: &str) -> Result<()> {
    ensure!(
        Uuid::parse_str(value)?.to_string() == value,
        "identifier must be a canonical UUID"
    );
    Ok(())
}
fn digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid SHA256 identifier"
    );
    Ok(())
}
fn scalar(value: u64) -> Result<i64> {
    Ok(i64::try_from(value)?)
}

struct Core {
    view: VfsView,
    binding: ObservationBinding,
    binding_id: String,
    policy: ImportPolicy,
    limits: RpcLimits,
    authority: Arc<dyn AttemptAuthority>,
    witness: Option<Arc<dyn AdoptionWitness>>,
    nonces: Mutex<BTreeSet<String>>,
}
struct NonceGuard<'a> {
    core: &'a Core,
    nonce: String,
}
struct BindingStage<'a> {
    store: &'a LocalStore,
    id: String,
}
impl Drop for BindingStage<'_> {
    fn drop(&mut self) {
        // Successful publication already consumed the lease. On failure/crash,
        // abandoned chunks are collectible after this lease is removed/expires.
        let _ = self.store.abandon_staging(&self.id);
    }
}
impl Drop for NonceGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut nonces) = self.core.nonces.lock() {
            nonces.remove(&self.nonce);
        }
    }
}
impl Core {
    fn own_nonce(&self, nonce: &str) -> Result<NonceGuard<'_>> {
        let mut nonces = self
            .nonces
            .lock()
            .map_err(|_| problem("unavailable", "workspace nonce registry unavailable"))?;
        if !nonces.insert(nonce.into()) {
            return Err(problem("busy", "observation operation is still settling"));
        }
        Ok(NonceGuard {
            core: self,
            nonce: nonce.into(),
        })
    }
    fn new(view: VfsView, config: RpcConfig) -> Result<Self> {
        config.limits.validate()?;
        config.profile.validate()?;
        ensure!(
            config.profile.kind == ObjectKind::Blob && config.profile.size_bytes <= 4 * 1024 * 1024,
            "workspace profile exceeds qualified schema/bound"
        );
        let store = view.manager().store().clone();
        let stage = BindingStage {
            store: &store,
            id: store.begin_staging(Duration::from_secs(60))?.id,
        };
        let profile: WorkspaceProfile =
            serde_json::from_slice(&store.read_object(&config.profile)?)?;
        ensure!(
            profile
                .prepared
                .len()
                .saturating_add(profile.scratch.len())
                .saturating_add(profile.vcs_local.len())
                <= 4096,
            "workspace partition count exceeds4096"
        );
        profile.validate()?;
        let policy = profile.source_exclusions();
        let cursor = view.dirty_cursor()?;
        let binding = ObservationBinding {
            format_version: 1,
            attempt: config.attempt,
            view_id: cursor.view_id,
            owner: cursor.owner,
            namespace: cursor.namespace,
            profile: config.profile,
            exclusion_policy_digest: super::exclusion_policy_digest(&policy)?,
        };
        binding.validate()?;
        config.authority.validate(&binding)?;
        let binding_id = binding.id()?;
        let core = Self {
            view,
            binding,
            binding_id,
            policy,
            limits: config.limits,
            authority: config.authority,
            witness: config.adoption_witness,
            nonces: Mutex::new(BTreeSet::new()),
        };
        {
            let c = core.view.db()?;
            c.execute_batch("CREATE TABLE IF NOT EXISTS workspace_rpc_bindings(id TEXT PRIMARY KEY,body TEXT NOT NULL);CREATE TABLE IF NOT EXISTS workspace_rpc_observations(nonce TEXT PRIMARY KEY,binding TEXT NOT NULL,state TEXT NOT NULL,result TEXT);CREATE TABLE IF NOT EXISTS workspace_rpc_adoptions(operation TEXT PRIMARY KEY,binding TEXT NOT NULL,digest TEXT NOT NULL,result TEXT NOT NULL);CREATE TABLE IF NOT EXISTS workspace_rpc_heads(binding TEXT PRIMARY KEY,result TEXT NOT NULL);")?;
            let body = String::from_utf8(canonical_json(&core.binding)?)?;
            let existing: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM workspace_rpc_bindings WHERE id=?)",
                [&core.binding_id],
                |r| r.get(0),
            )?;
            let count: i64 =
                c.query_row("SELECT count(*) FROM workspace_rpc_bindings", [], |r| {
                    r.get(0)
                })?;
            ensure!(
                existing || count < 64,
                "workspace RPC attempt binding budget exhausted"
            );
            c.execute(
                "INSERT OR IGNORE INTO workspace_rpc_bindings VALUES(?,?)",
                params![core.binding_id, body],
            )?;
            let stored: String = c.query_row(
                "SELECT body FROM workspace_rpc_bindings WHERE id=?",
                [&core.binding_id],
                |r| r.get(0),
            )?;
            ensure!(stored == body, "RPC binding record differs");
        }
        // SQLite is not a CAS retention edge. Retain the immutable profile and
        // interpretation binding before exposing an endpoint. Tokens never go
        // into CAS. This conservative pin survives close/restart, as do scoped
        // observation receipts; a future trusted history-retirement authority
        // may retire a whole binding only after its durable users are gone.
        let receipt = ReferencedMetadata {
            format_version: 1,
            references: vec![core.binding.profile.clone()],
            payload: serde_json::to_value(&core.binding)?,
        };
        let bytes = canonical_json(&receipt)?;
        let root = ObjectRef::from_bytes(ObjectKind::ActionResult, &bytes);
        store.receive_staged(&stage.id, &root, &bytes)?;
        store.publish(
            &stage.id,
            &format!(
                "vfs:rpc-binding:{}:{}",
                core.binding.view_id, core.binding_id
            ),
            &root,
        )?;
        Ok(core)
    }
    fn validate(&self) -> Result<()> {
        self.authority.validate(&self.binding).map_err(|_| {
            problem(
                "stale_attempt",
                "workspace attempt authority is no longer current",
            )
        })?;
        let cursor = self.view.dirty_cursor()?;
        ensure!(
            cursor.view_id == self.binding.view_id
                && cursor.owner == self.binding.owner
                && cursor.namespace == self.binding.namespace,
            "workspace observation identity changed"
        );
        Ok(())
    }
    fn head_locked(&self, c: &rusqlite::Connection) -> Result<Option<AdoptionReceipt>> {
        let value: Option<String> = c
            .query_row(
                "SELECT result FROM workspace_rpc_heads WHERE binding=?",
                [&self.binding_id],
                |r| r.get(0),
            )
            .optional()?;
        value
            .map(|value| Ok(serde_json::from_str(&value)?))
            .transpose()
    }
    fn owned_locked(
        &self,
        c: &rusqlite::Connection,
        nonce: &str,
    ) -> Result<(String, Option<VfsObservation>)> {
        uuid(nonce)?;
        let value: Option<(String, String, Option<String>)> = c
            .query_row(
                "SELECT binding,state,result FROM workspace_rpc_observations WHERE nonce=?",
                [nonce],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (binding, state, result) = value
            .ok_or_else(|| problem("not_found", "observation is not owned by this attempt"))?;
        if binding != self.binding_id {
            return Err(problem(
                "not_found",
                "observation is not owned by this attempt",
            ));
        }
        let result = result
            .map(|value| -> Result<VfsObservation> {
                let value = serde_json::from_str(&value)?;
                self.check_observation(nonce, &value)?;
                Ok(value)
            })
            .transpose()?;
        Ok((state, result))
    }
    fn check_observation(&self, nonce: &str, value: &VfsObservation) -> Result<()> {
        value.directory.validate()?;
        ensure!(
            value.format_version == 1
                && value.request_nonce == nonce
                && value.view_id == self.binding.view_id
                && value.owner == self.binding.owner
                && value.begin.view_id == value.view_id
                && value.end.view_id == value.view_id
                && value.begin.owner == value.owner
                && value.end.owner == value.owner
                && value.begin.namespace == self.binding.namespace
                && value.end.namespace == self.binding.namespace
                && value.begin.generation <= value.end.generation
                && value.exclusion_policy_digest == self.binding.exclusion_policy_digest
                && value.consistency == ObservationConsistency::LiveBestEffort
                && value.directory.kind == ObjectKind::Directory
                && value.pin_id == format!("vfs:observe:{}:{nonce}", self.binding.view_id),
            "observation receipt differs from its scoped binding"
        );
        Ok(())
    }
    fn inspect(&self, nonce: &str) -> Result<ObservationReceipt> {
        let c = self.view.db()?;
        let (state, mut observation) = self.owned_locked(&c, nonce)?;
        // Repair only a completed durable VFS receipt. Inspect never captures a
        // new root or guesses a result from an uncommitted/pending capture.
        let mut state = state;
        if state == "pending" && observation.is_none() {
            let result: Option<String> = c
                .query_row(
                    "SELECT result FROM observations WHERE nonce=? AND status='complete'",
                    [nonce],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if let Some(result) = result {
                let value: VfsObservation = serde_json::from_str(&result)?;
                self.check_observation(nonce, &value)?;
                c.execute("UPDATE workspace_rpc_observations SET state='retained',result=? WHERE nonce=? AND binding=? AND state='pending'", params![result, nonce, self.binding_id])?;
                state = "retained".into();
                observation = Some(value);
            }
        }
        let current = self
            .head_locked(&c)?
            .is_some_and(|head| head.current == nonce);
        drop(c);
        let pin = format!("vfs:observe:{}:{nonce}", self.binding.view_id);
        let retained = self
            .view
            .manager()
            .store()
            .retention()?
            .pins
            .get(&pin)
            .cloned();
        if let Some(value) = &observation {
            ensure!(
                retained.is_none() || retained.as_ref() == Some(&value.directory),
                "observation pin differs from its receipt"
            );
        }
        let state = match state.as_str() {
            "pending" => ReceiptState::Pending,
            "retained" => ReceiptState::Retained,
            "adopted" => ReceiptState::Adopted,
            "retiring" => ReceiptState::Retiring,
            "released" => ReceiptState::Released,
            _ => bail!("invalid observation lifecycle state"),
        };
        ensure!(
            matches!(
                state,
                ReceiptState::Pending | ReceiptState::Released | ReceiptState::Retiring
            ) || retained.is_some(),
            "active observation lost retention"
        );
        Ok(ObservationReceipt {
            nonce: nonce.into(),
            state,
            observation,
            retained: retained.is_some(),
            current,
        })
    }
    fn observe(&self, nonce: &str, cancelled: &dyn Fn() -> bool) -> Result<ObservationReceipt> {
        uuid(nonce)?;
        let _nonce = self.own_nonce(nonce)?;
        ensure!(!cancelled(), "observation request cancelled before capture");
        {
            let mut c = self.view.db()?;
            let tx = c.transaction()?;
            let existing: Option<String> = tx
                .query_row(
                    "SELECT binding FROM workspace_rpc_observations WHERE nonce=?",
                    [nonce],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(binding) = existing {
                ensure!(
                    binding == self.binding_id,
                    "nonce belongs to another attempt"
                );
            } else {
                let foreign: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM observations WHERE nonce=?)",
                    [nonce],
                    |r| r.get(0),
                )?;
                ensure!(
                    !foreign,
                    "nonce belongs to an observation outside this RPC attempt"
                );
                let (total, live): (i64, i64) = tx.query_row("SELECT count(*),coalesce(sum(state!='released'),0) FROM workspace_rpc_observations", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
                ensure!(
                    total < self.limits.max_receipts as i64
                        && live < self.limits.max_live_observations as i64,
                    "observation retention budget exhausted"
                );
                tx.execute(
                    "INSERT INTO workspace_rpc_observations VALUES(?,?,'pending',NULL)",
                    params![nonce, self.binding_id],
                )?;
            }
            let (state, _) = self.owned_locked(&tx, nonce)?;
            ensure!(
                state != "released" && state != "retiring",
                "observation nonce has been retired"
            );
            tx.commit()?;
        }
        let value = self.view.observe_source_cancellable(
            &self.policy,
            nonce,
            &self.limits.observation,
            cancelled,
        )?;
        self.check_observation(nonce, &value)?;
        // A publication racing cancellation stays retained and inspectable.
        {
            let c = self.view.db()?;
            let (state, previous) = self.owned_locked(&c, nonce)?;
            ensure!(
                state != "released" && state != "retiring",
                "observation was concurrently retired"
            );
            ensure!(
                previous.is_none_or(|previous| previous == value),
                "observation result changed"
            );
            c.execute("UPDATE workspace_rpc_observations SET state=CASE WHEN state='pending' THEN 'retained' ELSE state END,result=? WHERE nonce=? AND binding=?", params![serde_json::to_string(&value)?, nonce, self.binding_id])?;
        }
        self.inspect(nonce)
    }
    fn release(&self, nonce: &str) -> Result<ObservationReceipt> {
        uuid(nonce)?;
        let _nonce = self.own_nonce(nonce)?;
        {
            let mut c = self.view.db()?;
            let tx = c.transaction()?;
            let (state, _) = self.owned_locked(&tx, nonce)?;
            ensure!(
                !self
                    .head_locked(&tx)?
                    .is_some_and(|head| head.current == nonce),
                "current durable WC observation is protected"
            );
            ensure!(
                state != "adopted" && state != "retiring",
                "adopted observations require trusted replacement reconciliation"
            );
            tx.execute("UPDATE workspace_rpc_observations SET state='released' WHERE nonce=? AND binding=?", params![nonce, self.binding_id])?;
            tx.commit()?;
        }
        let exists = {
            let c = self.view.db()?;
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM observations WHERE nonce=?)",
                [nonce],
                |r| r.get::<_, bool>(0),
            )?
        };
        if exists {
            self.view.release_observation(nonce)?;
        }
        self.inspect(nonce)
    }
    fn drain_retiring(&self) -> Result<()> {
        let nonces = {
            let c = self.view.db()?;
            let mut q = c.prepare(
                "SELECT nonce FROM workspace_rpc_observations WHERE binding=? AND state='retiring'",
            )?;
            let nonces = q
                .query_map([&self.binding_id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            nonces
        };
        for nonce in nonces {
            self.validate()?;
            {
                let c = self.view.db()?;
                ensure!(
                    !self
                        .head_locked(&c)?
                        .is_some_and(|head| head.current == nonce),
                    "retiring observation is current"
                );
            }
            self.view.release_observation(&nonce)?;
            let c = self.view.db()?;
            c.execute("UPDATE workspace_rpc_observations SET state='released' WHERE nonce=? AND binding=? AND state='retiring'", params![nonce, self.binding_id])?;
        }
        Ok(())
    }
    fn adopt(
        &self,
        operation: &str,
        expected: &Option<String>,
        nonce: &str,
        evidence: &AdoptionEvidence,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<AdoptionReceipt> {
        let witness = self.witness.as_ref().ok_or_else(|| {
            problem(
                "unsupported",
                "durable WC adoption has no qualified trusted witness",
            )
        })?;
        uuid(operation)?;
        uuid(nonce)?;
        if let Some(previous) = expected {
            uuid(previous)?;
        }
        digest(&evidence.state_sha256)?;
        ensure!(
            canonical_json(&evidence.tree)?.len() <= 8192,
            "WC tree evidence exceeds bound"
        );
        let request_hash = hex::encode(Sha256::digest(canonical_json(&(
            operation, expected, nonce, evidence,
        ))?));
        let existing = {
            let c = self.view.db()?;
            c.query_row(
                "SELECT binding,digest,result FROM workspace_rpc_adoptions WHERE operation=?",
                [operation],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        };
        if let Some((binding, hash, result)) = existing {
            ensure!(
                binding == self.binding_id && hash == request_hash,
                "adoption operation is bound to another request"
            );
            let receipt = serde_json::from_str(&result)?;
            let _ = self.drain_retiring();
            return Ok(receipt);
        }
        let _nonce = self.own_nonce(nonce)?;
        ensure!(!cancelled(), "adoption cancelled before witness");
        // Another identical caller may have committed between our first lookup
        // and acquiring this nonce. Return its original receipt, never re-adopt.
        let raced = {
            let c = self.view.db()?;
            c.query_row(
                "SELECT binding,digest,result FROM workspace_rpc_adoptions WHERE operation=?",
                [operation],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        };
        if let Some((binding, hash, result)) = raced {
            ensure!(
                binding == self.binding_id && hash == request_hash,
                "adoption operation is bound to another request"
            );
            let receipt = serde_json::from_str(&result)?;
            let _ = self.drain_retiring();
            return Ok(receipt);
        }
        let inspected = self.inspect(nonce)?;
        ensure!(
            inspected.state == ReceiptState::Retained && inspected.retained,
            "only a new retained observation can be adopted"
        );
        let observation = inspected
            .observation
            .context("observation outcome is pending")?;
        let _guard = witness.verify(&self.binding, &observation, evidence)?;
        self.validate()?;
        ensure!(!cancelled(), "adoption cancelled before head comparison");
        let receipt = {
            let mut c = self.view.db()?;
            let tx = c.transaction()?;
            let head = self.head_locked(&tx)?;
            ensure!(
                head.as_ref().map(|head| &head.current) == expected.as_ref(),
                "durable WC observation head changed"
            );
            let (state, stored) = self.owned_locked(&tx, nonce)?;
            ensure!(
                state == "retained" && stored.as_ref() == Some(&observation),
                "observation is no longer a new retained candidate"
            );
            let count: i64 =
                tx.query_row("SELECT count(*) FROM workspace_rpc_adoptions", [], |r| {
                    r.get(0)
                })?;
            ensure!(
                count < self.limits.max_receipts as i64,
                "adoption receipt budget exhausted"
            );
            let sequence = head
                .as_ref()
                .map_or(0, |head| head.sequence)
                .checked_add(1)
                .context("adoption sequence exhausted")?;
            scalar(sequence)?;
            let receipt = AdoptionReceipt {
                operation_id: operation.into(),
                previous: expected.clone(),
                current: nonce.into(),
                sequence,
                state_sha256: evidence.state_sha256.clone(),
                tree_sha256: hex::encode(Sha256::digest(canonical_json(&evidence.tree)?)),
            };
            let encoded = serde_json::to_string(&receipt)?;
            tx.execute(
                "INSERT INTO workspace_rpc_adoptions VALUES(?,?,?,?)",
                params![operation, self.binding_id, request_hash, encoded],
            )?;
            tx.execute("INSERT INTO workspace_rpc_heads VALUES(?,?) ON CONFLICT(binding) DO UPDATE SET result=excluded.result", params![self.binding_id, encoded])?;
            tx.execute(
                "UPDATE workspace_rpc_observations SET state='adopted' WHERE nonce=? AND binding=?",
                params![nonce, self.binding_id],
            )?;
            if let Some(previous) = expected {
                let (state, _) = self.owned_locked(&tx, previous)?;
                ensure!(state == "adopted", "previous WC observation is not adopted");
                tx.execute("UPDATE workspace_rpc_observations SET state='retiring' WHERE nonce=? AND binding=?", params![previous, self.binding_id])?;
            }
            tx.commit()?;
            receipt
        };
        // The durable adoption is already recorded. Cleanup failures retain
        // both roots; same-operation retry reconciles without re-adopting.
        let _ = self.drain_retiring();
        Ok(receipt)
    }
}
