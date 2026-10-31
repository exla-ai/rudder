//! Per-attempt agent ingress outside the worker mount namespace. The worker
//! receives one protected identity and socket, never authority database paths.
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use rudder_engine::agent_api::{AgentApi, AgentIdentityFile};
use rudder_engine::journal::MessageEnvelope;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_REQUEST: usize = 256 * 1024;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const MAX_CALLS: usize = 4;
const MAX_CONNECTIONS: usize = 8;
const CALL_DEADLINE: Duration = Duration::from_secs(32);
const CONNECT_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BrokerBinding {
    pub project_id: String,
    pub run_id: String,
    pub run_epoch: u64,
    pub owner_epoch: u64,
}
impl From<&AgentIdentityFile> for BrokerBinding {
    fn from(value: &AgentIdentityFile) -> Self {
        Self {
            project_id: value.project_id.clone(),
            run_id: value.run_id.clone(),
            run_epoch: value.run_epoch,
            owner_epoch: value.owner_epoch,
        }
    }
}
#[derive(Debug, Clone)]
pub(crate) struct BrokerConfig {
    pub journal_path: PathBuf,
    pub identity_file: PathBuf,
    pub socket_path: PathBuf,
}

/// Pure address derivation; preparing credentials/listener is explicit. A short
/// per-user fallback avoids Unix sun_path limits for deeply nested checkouts.
pub(crate) fn socket_path(home: &Path, project: &str, run: &str, attempt: &str) -> PathBuf {
    let input = format!("{}\0{project}\0{run}\0{attempt}", home.display());
    let key = hex_digest(input.as_bytes());
    let root = home.join("fabric/agent-brokers");
    let path = root.join(&key[..24]).join("broker.sock");
    if path.as_os_str().as_encoded_bytes().len() < 100 {
        path
    } else {
        PathBuf::from(format!("/tmp/rudder-agent-brokers-{}", unsafe {
            libc::geteuid()
        }))
        .join(&key[..24])
        .join("broker.sock")
    }
}

fn private_directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o700,
        "broker directory is not privately owned"
    );
    // Refuse an existing shared/public directory; never chmod an arbitrary
    // caller path (in particular /tmp or /) into a private broker directory.
    Ok(())
}
fn endpoint_directory(socket: &Path) -> Result<&Path> {
    ensure!(
        socket.file_name().is_some_and(|name| name == "broker.sock"),
        "broker socket must be named broker.sock"
    );
    let parent = socket.parent().context("broker socket directory")?;
    // The base is private as well as its leaf, including the /tmp fallback.
    private_directory(parent.parent().context("broker socket base")?)?;
    private_directory(parent)?;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        ensure!(
            name == "identity.json" || name == "broker.sock",
            "broker projection contains an unexpected entry"
        );
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink() && metadata.uid() == unsafe { libc::geteuid() },
            "unsafe broker projection entry"
        );
    }
    Ok(parent)
}

/// Atomic rotation never places a temporary credential in the directory bound
/// into the worker. The sibling temp is on the same filesystem as its target.
pub(crate) fn project_identity(socket: &Path, identity: &AgentIdentityFile) -> Result<PathBuf> {
    let parent = endpoint_directory(socket)?;
    validate_identity(identity)?;
    let destination = parent.join("identity.json");
    if destination.exists() {
        ensure!(
            BrokerBinding::from(&read_identity(&destination)?) == BrokerBinding::from(identity),
            "cannot replace a projected agent attempt; allocate a new endpoint"
        );
    }
    let temporary = parent
        .parent()
        .unwrap()
        .join(format!(".identity-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, identity)?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)?;
        fs::File::open(parent)?.sync_all()?;
        fs::File::open(parent.parent().unwrap())?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(destination)
}
fn validate_identity(identity: &AgentIdentityFile) -> Result<()> {
    ensure!(
        identity.format_version == 1
            && !identity.project_id.is_empty()
            && !identity.run_id.is_empty()
            && identity.run_epoch > 0
            && identity.owner_epoch > 0
            && !identity.token.is_empty(),
        "invalid agent broker identity"
    );
    Ok(())
}
fn read_identity(path: &Path) -> Result<AgentIdentityFile> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .context("open protected agent identity")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.mode() & 0o777 == 0o600
            && metadata.uid() == unsafe { libc::geteuid() },
        "agent identity must be an owned single-link mode 0600 file"
    );
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16 * 1024,
        "agent identity exceeds byte limit"
    );
    let identity: AgentIdentityFile =
        serde_json::from_slice(&bytes).context("invalid protected identity")?;
    validate_identity(&identity)?;
    Ok(identity)
}
fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn token_equal(a: &str, b: &str) -> bool {
    let a = Sha256::digest(a.as_bytes());
    let b = Sha256::digest(b.as_bytes());
    a.iter()
        .zip(b)
        .fold(0_u8, |different, (a, b)| different | (*a ^ b))
        == 0
}

struct WorkDirectory(PathBuf);
impl Drop for WorkDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}
struct Snapshot {
    api: AgentApi,
    token_hash: String,
    path: PathBuf,
    _directory: Arc<WorkDirectory>,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    format_version: u32,
    id: String,
    identity: AgentIdentityFile,
    operation: Operation,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Authenticate,
    AuthorizeWorkspace { binding: rudder_engine::vfs::rpc::ObservationBinding },
    AuthorizePartialWorkspace { binding: rudder_engine::vfs::partial_rpc::PartialBinding },
    Call { tool: String, arguments: Value },
    Pending,
    Cancel { request_id: String },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    format_version: u32,
    id: String,
    result: Option<Value>,
    error: Option<WireError>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireError {
    code: String,
    message: String,
}
impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for WireError {}
fn problem(code: &str, message: &str) -> anyhow::Error {
    anyhow::Error::new(WireError {
        code: code.into(),
        message: message.into(),
    })
}
pub(crate) fn retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<WireError>()
        .is_some_and(|error| matches!(error.code.as_str(), "busy" | "unavailable" | "timeout"))
}
fn wire_error(error: &anyhow::Error) -> WireError {
    if let Some(error) = error.downcast_ref::<WireError>() {
        return error.clone();
    }
    let busy = error.chain().any(|cause|cause.downcast_ref::<rusqlite::Error>().is_some_and(|error|matches!(error,rusqlite::Error::SqliteFailure(code,_) if matches!(code.code,rusqlite::ErrorCode::DatabaseBusy|rusqlite::ErrorCode::DatabaseLocked))));
    if busy {
        WireError {
            code: "busy".into(),
            message: "agent authority is temporarily busy".into(),
        }
    } else {
        WireError { code:"rejected".into(), message:"agent broker rejected the request; inspect scoped receipts before retrying an uncertain effect".into() }
    }
}

struct Inner {
    config: BrokerConfig,
    binding: BrokerBinding,
    directory: Arc<WorkDirectory>,
    current: Mutex<Option<Arc<Snapshot>>>,
    active: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
    connections: Mutex<BTreeMap<u64, UnixStream>>,
    count: AtomicUsize,
    sequence: AtomicU64,
    stopped: AtomicBool,
    closed: AtomicBool,
    owned_calls: Option<Arc<OwnedBrokerCalls>>,
    #[cfg(test)]
    api_probe: Mutex<Option<owned_thread_tests::ApiProbe>>,
}
impl Inner {
    fn authenticate(&self, offered: &AgentIdentityFile) -> Result<Arc<Snapshot>> {
        validate_identity(offered)?;
        let current = read_identity(&self.config.identity_file)?;
        ensure!(
            BrokerBinding::from(&current) == self.binding
                && BrokerBinding::from(offered) == self.binding
                && token_equal(&current.token, &offered.token),
            "agent broker authentication rejected"
        );
        let hash = hex_digest(current.token.as_bytes());
        let mut snapshot = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("broker snapshot lock failed"))?;
        if snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.token_hash != hash)
        {
            let path = self
                .directory
                .0
                .join(format!("{}.json", uuid::Uuid::new_v4()));
            let result = (|| -> Result<AgentApi> {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&path)?;
                serde_json::to_writer(&mut file, &current)?;
                file.flush()?;
                AgentApi::open(&self.config.journal_path, &path)
            })();
            match result {
                Ok(api) => {
                    *snapshot = Some(Arc::new(Snapshot {
                        api,
                        token_hash: hash,
                        path,
                        _directory: self.directory.clone(),
                    }))
                }
                Err(error) => {
                    let _ = fs::remove_file(path);
                    return Err(error);
                }
            }
        }
        let snapshot = snapshot.as_ref().unwrap().clone();
        // Check revocation/current epochs even when the cached token matches.
        let actual = snapshot.api.authenticated_identity()?;
        ensure!(
            actual.project_id == self.binding.project_id
                && actual.run_id == self.binding.run_id
                && actual.run_epoch == self.binding.run_epoch
                && actual.owner_epoch == self.binding.owner_epoch,
            "agent broker authority changed"
        );
        Ok(snapshot)
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Ok(active) = self.active.lock() {
            for cancel in active.values() {
                cancel.store(true, Ordering::Relaxed);
            }
        }
        if let Ok(connections) = self.connections.lock() {
            for connection in connections.values() {
                let _ = connection.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

pub(crate) struct AgentBroker {
    inner: Arc<Inner>,
}
impl AgentBroker {
    pub(crate) fn spawn(config: BrokerConfig) -> Result<Self> {
        Self::spawn_inner(config, None)
    }
    /// Opt-in source preparation only. The preexisting owner keeps partially
    /// constructed state and every actual thread handle on failure/panic.
    pub(crate) fn spawn_retaining(config: BrokerConfig, owner: &mut BrokerThreadOwner) -> Result<Self> {
        ensure!(owner.inner.is_none(), "broker thread owner already used");
        Self::spawn_inner(config, Some(owner))
    }
    fn spawn_inner(config: BrokerConfig, mut owner: Option<&mut BrokerThreadOwner>) -> Result<Self> {
        let parent = endpoint_directory(&config.socket_path)?;
        // Keep ownership outside the mounted two-entry projection. The lock
        // serializes stale-socket removal against concurrent broker restarts.
        let lock_path = parent.parent().unwrap().join(format!(
            "{}.lock",
            parent.file_name().unwrap().to_string_lossy()
        ));
        let ownership = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(lock_path)?;
        let lock_metadata = ownership.metadata()?;
        ensure!(
            lock_metadata.is_file()
                && lock_metadata.nlink() == 1
                && lock_metadata.uid() == unsafe { libc::geteuid() },
            "unsafe broker ownership lock"
        );
        ownership
            .try_lock_exclusive()
            .context("another broker owns this attempt endpoint")?;
        ensure!(
            config.socket_path.as_os_str().as_encoded_bytes().len() < 104,
            "agent broker socket path exceeds platform limit"
        );
        ensure!(
            config.identity_file.canonicalize()? == parent.join("identity.json").canonicalize()?,
            "broker identity must be its dedicated projection"
        );
        let identity = read_identity(&config.identity_file)?;
        let journal = fs::symlink_metadata(&config.journal_path)?;
        ensure!(
            journal.is_file() && journal.uid() == unsafe { libc::geteuid() },
            "agent broker requires an existing owned authority journal"
        );
        let private = config
            .journal_path
            .parent()
            .context("journal directory")?
            .join("broker-work");
        private_directory(&private)?;
        let private = private.join(uuid::Uuid::new_v4().to_string());
        private_directory(&private)?;
        let inner = Arc::new(Inner {
            config: config.clone(),
            binding: BrokerBinding::from(&identity),
            directory: Arc::new(WorkDirectory(private)),
            current: Mutex::new(None),
            active: Mutex::new(BTreeMap::new()),
            connections: Mutex::new(BTreeMap::new()),
            count: AtomicUsize::new(0),
            sequence: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            owned_calls: owner.as_ref().map(|owner| Arc::clone(&owner.calls)),
            #[cfg(test)]
            api_probe: Mutex::new(None),
        });
        if let Some(owner) = owner.as_mut() { owner.inner = Some(Arc::clone(&inner)); }
        inner.authenticate(&identity)?;
        if let Ok(metadata) = fs::symlink_metadata(&config.socket_path) {
            ensure!(
                metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
                "refusing to replace an unsafe broker endpoint"
            );
            match UnixStream::connect(&config.socket_path) {
                Ok(_) => bail!("a broker already owns this endpoint"),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(&config.socket_path)?
                }
                Err(_) => bail!("existing broker endpoint could not be safely classified"),
            }
        }
        let listener = UnixListener::bind(&config.socket_path)?;
        fs::set_permissions(&config.socket_path, fs::Permissions::from_mode(0o600))?;
        let metadata = fs::symlink_metadata(&config.socket_path)?;
        let owned = (metadata.dev(), metadata.ino());
        listener.set_nonblocking(true)?;
        let worker = inner.clone();
        let acceptor = std::thread::Builder::new()
            .name("rudder-agent-broker".into())
            .spawn(move || {
                let _ownership = ownership;
                while !worker.stopped.load(Ordering::Relaxed) {
                    if let Some(calls) = &worker.owned_calls {
                        match calls.reap_finished() {
                            Ok(true) => {}
                            Ok(false) => {
                                // TLS/unjoined handlers still own every slot.
                                // Leave new connections in the bounded backlog.
                                std::thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            Err(_) => { worker.stop(); break; }
                        }
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if worker.count.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
                                worker.count.fetch_sub(1, Ordering::Relaxed);
                                drop(stream);
                                continue;
                            }
                            let id = worker.sequence.fetch_add(1, Ordering::Relaxed);
                            let clone = match stream.try_clone() {
                                Ok(clone) => clone,
                                Err(_) => {
                                    worker.count.fetch_sub(1, Ordering::Relaxed);
                                    continue;
                                }
                            };
                            if let Ok(mut connections) = worker.connections.lock() {
                                connections.insert(id, clone);
                            } else {
                                worker.count.fetch_sub(1, Ordering::Relaxed);
                                continue;
                            }
                            let connection = worker.clone();
                            let spawned = std::thread::Builder::new()
                                .name("rudder-agent-call".into())
                                .spawn(move || {
                                    connection_call(&connection, stream);
                                    if let Ok(mut connections) = connection.connections.lock() {
                                        connections.remove(&id);
                                    }
                                    connection.count.fetch_sub(1, Ordering::Relaxed);
                                });
                            match spawned {
                                Ok(handle) => {
                                    if let Some(calls) = &worker.owned_calls {
                                        // The sole acceptor reserved capacity above.
                                        // Poison still retains this new actual handle.
                                        if !calls.track(handle) { worker.stop(); }
                                    }
                                }
                                Err(_) => {
                                    if let Ok(mut connections) = worker.connections.lock() { connections.remove(&id); }
                                    worker.count.fetch_sub(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(_) => break,
                    }
                }
                worker.stop();
                drop(listener);
                if fs::symlink_metadata(&worker.config.socket_path)
                    .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == owned)
                {
                    let _ = fs::remove_file(&worker.config.socket_path);
                }
                worker.closed.store(true, Ordering::Relaxed);
            })?;
        if let Some(owner) = owner { owner.acceptor = Some(acceptor); }
        Ok(Self { inner })
    }
    pub(crate) fn socket_path(&self) -> &Path {
        &self.inner.config.socket_path
    }
    pub(crate) fn binding(&self) -> &BrokerBinding {
        &self.inner.binding
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Relaxed)
    }
    pub(crate) fn close(&self) {
        self.inner.stop();
    }
    pub(crate) fn close_callback(&self) -> Arc<dyn Fn() + Send + Sync> {
        let weak = Arc::downgrade(&self.inner);
        Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.stop();
            }
        })
    }
}
impl Drop for AgentBroker {
    fn drop(&mut self) {
        if self.inner.owned_calls.is_some() {
            self.inner.stopped.store(true, Ordering::Release);
        } else { self.close(); }
    }
}

// Bound currently unjoined requests, including request TLS teardown. The sole
// acceptor reaps before admission, so sequential requests have no lifetime cap.
const MAX_OWNED_UNJOINED: usize = MAX_CONNECTIONS;
type BrokerPanic = Box<dyn std::any::Any + Send + 'static>;
struct OwnedBrokerCalls {
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    uncertain: AtomicBool,
    // Retain panic payloads rather than running arbitrary payload Drop while
    // other request handles are being joined. A panic stops further admission;
    // each bounded handler/API worker plus the acceptor can retain a payload.
    panics: Mutex<Vec<BrokerPanic>>,
}
impl OwnedBrokerCalls {
    fn new() -> Self {
        Self { handles:Mutex::new(Vec::with_capacity(MAX_OWNED_UNJOINED)),
            uncertain:AtomicBool::new(false), panics:Mutex::new(Vec::with_capacity(2 * MAX_OWNED_UNJOINED + 1)) }
    }
    fn record_panic(&self, payload: BrokerPanic) {
        self.uncertain.store(true, Ordering::Release);
        self.panics.lock().unwrap_or_else(|p|p.into_inner()).push(payload);
    }
    fn handles(&self) -> std::sync::MutexGuard<'_, Vec<std::thread::JoinHandle<()>>> {
        self.handles.lock().unwrap_or_else(|poison| {
            self.uncertain.store(true, Ordering::Release);
            poison.into_inner()
        })
    }
    /// Only the actual acceptor calls this, off-App. is_finished selects a
    /// candidate; the real join (possibly blocked by TLS) retains its slot on
    /// the acceptor's stack. Shutdown must join that acceptor before draining.
    fn reap_finished(&self) -> Result<bool> {
        loop {
            ensure!(!self.uncertain.load(Ordering::Acquire), "broker thread ownership uncertain");
            let candidate = {
                let mut handles = self.handles();
                ensure!(!self.uncertain.load(Ordering::Acquire), "broker thread ownership poisoned");
                match handles.iter().position(|handle|handle.is_finished()) {
                    Some(index) => handles.swap_remove(index),
                    None => return Ok(handles.len() < MAX_OWNED_UNJOINED),
                }
            };
            if let Err(payload) = candidate.join() {
                self.record_panic(payload);
                anyhow::bail!("reaped broker handler panicked; explicit recovery required");
            }
        }
    }
    /// Called only after the sole acceptor established a slot. No allocation
    /// or fallible lock step may discard the newly spawned request handle.
    fn track(&self, handle: std::thread::JoinHandle<()>) -> bool {
        self.handles().push(handle);
        !self.uncertain.load(Ordering::Acquire)
    }
    fn join_remaining(&self) -> bool {
        let handles = std::mem::take(&mut *self.handles());
        for handle in handles {
            if let Err(payload) = handle.join() { self.record_panic(payload); }
        }
        !self.uncertain.load(Ordering::Acquire)
    }
}
#[must_use = "retain through explicit off-App await_closed"]
pub(crate) struct BrokerThreadOwner {
    inner: Option<Arc<Inner>>,
    calls: Arc<OwnedBrokerCalls>,
    acceptor: Option<std::thread::JoinHandle<()>>,
    joiner: Option<BrokerJoin>,
    settled: bool,
}
impl BrokerThreadOwner {
    pub(crate) fn new() -> Self {
        Self { inner: None, calls: Arc::new(OwnedBrokerCalls::new()),
            acceptor: None, joiner: None, settled: false }
    }
    pub(crate) fn close(&self) { if let Some(inner) = &self.inner { inner.stop(); } }
    pub(crate) fn await_closed(&mut self, timeout: Duration) -> Result<BrokerThreadsSettled> {
        ensure!(!self.settled, "broker settlement already returned");
        self.close();
        if self.acceptor.is_none() && self.joiner.is_none() {
            // No thread was successfully spawned; setup IO has already returned.
            ensure!(self.calls.handles().is_empty(),
                "broker call without acceptor ownership");
            ensure!(!self.calls.uncertain.load(Ordering::Acquire), "broker thread settlement is uncertain");
            self.settled = true;
            return Ok(BrokerThreadsSettled { _inner:self.inner.clone(), _joiner:None });
        }
        let end = Instant::now().checked_add(timeout).context("broker settlement overflow")?;
        if self.joiner.is_none() {
            let acceptor = Arc::new(Mutex::new(self.acceptor.take()));
            let joining = Arc::clone(&acceptor); let calls = Arc::clone(&self.calls);
            let (send, done) = mpsc::sync_channel(1);
            let thread = std::thread::Builder::new().name("rudder-source-broker-join".into()).spawn(move || {
                let acceptor = joining.lock().unwrap_or_else(|p|p.into_inner()).take().unwrap();
                if let Err(payload) = acceptor.join() { calls.record_panic(payload); }
                // The actual acceptor join proves no more request handles can appear.
                let success = calls.join_remaining();
                let _ = send.send(success);
            });
            match thread {
                Ok(thread) => self.joiner = Some(BrokerJoin { _thread:thread, done, joined:false, failed:false }),
                Err(error) => { self.acceptor = acceptor.lock().unwrap_or_else(|p|p.into_inner()).take(); return Err(error.into()); }
            }
        }
        let joiner = self.joiner.as_mut().unwrap();
        ensure!(!joiner.failed, "broker thread panic requires explicit recovery");
        if !joiner.joined {
            match joiner.done.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Ok(true) => joiner.joined = true,
                Err(mpsc::RecvTimeoutError::Timeout) => anyhow::bail!("broker callbacks/threads remain owned"),
                _ => { joiner.failed = true; anyhow::bail!("broker thread settlement is uncertain"); }
            }
        }
        if let Some(inner) = &self.inner {
            ensure!(inner.count.load(Ordering::Acquire) == 0
                && inner.connections.lock().map_err(|_| {
                    self.calls.uncertain.store(true, Ordering::Release);
                    anyhow::anyhow!("broker connection state poisoned")
                })?.is_empty()
                && inner.active.lock().map_err(|_| {
                    self.calls.uncertain.store(true, Ordering::Release);
                    anyhow::anyhow!("broker call state poisoned")
                })?.is_empty(),
                "broker requests remain owned");
        }
        ensure!(!self.calls.uncertain.load(Ordering::Acquire), "broker thread settlement is uncertain");
        self.settled = true;
        Ok(BrokerThreadsSettled { _inner:self.inner.clone(), _joiner:self.joiner.take() })
    }
}
impl Drop for BrokerThreadOwner {
    fn drop(&mut self) {
        if let Some(inner) = &self.inner { inner.stopped.store(true, Ordering::Release); }
    }
}
struct BrokerJoin { _thread:std::thread::JoinHandle<()>, done:mpsc::Receiver<bool>, joined:bool, failed:bool }
/// Proves actual acceptor, request and nested API joins, including their TLS.
/// The fixed join observer itself remains held here, matching source settlement.
pub(crate) struct BrokerThreadsSettled { _inner:Option<Arc<Inner>>, _joiner:Option<BrokerJoin> }

fn read_exact_until(
    stream: &mut UnixStream,
    bytes: &mut [u8],
    deadline: Instant,
    mut check: impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        check()?;
        if Instant::now() >= deadline {
            return Err(problem("timeout", "agent broker frame deadline exceeded"));
        }
        match stream.read(&mut bytes[offset..]) {
            Ok(0) => return Err(problem("unavailable","agent broker connection ended without confirmation; inspect scoped receipts before retrying an uncertain effect")),
            Ok(count) => offset += count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(problem("unavailable","agent broker transport failed without confirmation")),
        }
    }
    Ok(())
}
fn read_frame(
    stream: &mut UnixStream,
    limit: usize,
    deadline: Instant,
    mut check: impl FnMut() -> Result<()>,
) -> Result<Vec<u8>> {
    let mut prefix = [0_u8; 4];
    read_exact_until(stream, &mut prefix, deadline, &mut check)?;
    let length = u32::from_be_bytes(prefix) as usize;
    ensure!(
        length > 0 && length <= limit,
        "agent broker frame exceeds byte limit"
    );
    let mut bytes = vec![0; length];
    read_exact_until(stream, &mut bytes, deadline, check)?;
    Ok(bytes)
}
fn write_frame(stream: &mut UnixStream, value: &impl Serialize, limit: usize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= limit,
        "agent broker result exceeds frame limit; request fewer items"
    );
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}
#[cfg(target_os = "macos")]
fn disconnected(stream: &UnixStream) -> bool {
    // Darwin reports read-half EOF as POLLHUP. A peer that finished sending
    // its length-prefixed request may still be waiting for our response.
    // LOCAL_PEERPID instead checks the live Unix PCB connection: XNU retains
    // it on SHUT_WR and returns ENOTCONN after the peer fully disconnects.
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
    // Unknown probe errors are not proof of cancellation. Explicit cancel,
    // broker stop and the existing call deadline still bound every operation.
    result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTCONN)
}
#[cfg(not(target_os = "macos"))]
fn disconnected(stream: &UnixStream) -> bool {
    let mut fd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    (unsafe { libc::poll(&mut fd, 1, 0) }) >= 0
        && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}
fn connection_call(inner: &Arc<Inner>, stream: UnixStream) {
    let Some(calls) = &inner.owned_calls else {
        connection_request(inner, stream, None);
        return;
    };
    // This carrier is OUTSIDE the unwind boundary. Once an API worker exists,
    // neither a reply, error, disconnect nor handler panic may detach it.
    let mut api = OwnedApiCall::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        connection_request(inner, stream, Some(&mut api));
    }));
    if let Err(payload) = result {
        calls.record_panic(payload);
        inner.stop();
    }
    api.join(inner, calls); // OFF-APP actual join, including API-worker TLS.
}

#[derive(Default)]
struct OwnedApiCall {
    worker: Option<std::thread::JoinHandle<()>>,
    active_id: Option<String>,
    cancel: Option<Arc<AtomicBool>>,
}
impl OwnedApiCall {
    fn join(&mut self, inner: &Inner, calls: &OwnedBrokerCalls) {
        if let Some(cancel) = &self.cancel { cancel.store(true, Ordering::Relaxed); }
        if let Some(worker) = self.worker.take() {
            if let Err(payload) = worker.join() {
                calls.record_panic(payload);
                inner.stop();
            }
        }
        if let Some(id) = self.active_id.take() {
            let clean = match inner.active.lock() {
                Ok(mut active) => { active.remove(&id); true }
                Err(poison) => {
                    calls.uncertain.store(true, Ordering::Release);
                    poison.into_inner().remove(&id);
                    false
                }
            };
            if !clean { inner.stop(); }
        }
    }
}

fn connection_request(inner: &Arc<Inner>, mut stream: UnixStream, mut api_owner: Option<&mut OwnedApiCall>) {
    // BSD accept() inherits the listener's O_NONBLOCK flag; Linux does not.
    // Our per-connection worker uses blocking I/O with bounded timeouts. Without
    // clearing the flag, a response larger than the socket buffer is truncated
    // at the first EAGAIN even when the client is actively reading it.
    if stream
        .set_nonblocking(false)
        .and_then(|_| stream.set_read_timeout(Some(Duration::from_millis(20))))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_millis(500))))
        .is_err()
    {
        return;
    }
    let bytes = match read_frame(
        &mut stream,
        MAX_REQUEST,
        Instant::now() + Duration::from_secs(2),
        || {
            ensure!(!inner.stopped.load(Ordering::Relaxed), "broker closed");
            Ok(())
        },
    ) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    let request: Request = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(_) => return,
    };
    if request.format_version != 1 || uuid::Uuid::parse_str(&request.id).is_err() {
        return;
    }
    let id = request.id.clone();
    let result = (|| -> Result<Value> {
        let snapshot = inner.authenticate(&request.identity).map_err(|_| {
            problem(
                "unauthorized",
                "agent broker identity is revoked, stale or outside this attempt",
            )
        })?;
        if inner.stopped.load(Ordering::Relaxed) {
            return Err(problem("unavailable", "agent broker is closing"));
        }
        if let Operation::Authenticate = request.operation {
            return Ok(json!({"binding":inner.binding}));
        }
        if let Operation::AuthorizeWorkspace { ref binding } = request.operation {
            snapshot.api.authorize_workspace(binding)?;
            return Ok(json!({"binding":inner.binding,"workspace_scope":rudder_engine::execution::workspace_scope(binding)?}));
        }
        if let Operation::AuthorizePartialWorkspace { ref binding } = request.operation {
            snapshot.api.authorize_partial_workspace(binding)?;
            return Ok(json!({"binding":inner.binding,"partial_workspace_scope":rudder_engine::vfs::partial_rpc::partial_scope(binding)?}));
        }
        if let Operation::Cancel { request_id } = request.operation {
            if let Some(cancel) = inner
                .active
                .lock()
                .map_err(|_| anyhow::anyhow!("broker call lock failed"))?
                .get(&request_id)
            {
                cancel.store(true, Ordering::Relaxed);
            }
            return Ok(json!({"cancellation_requested":true}));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut active = inner
                .active
                .lock()
                .map_err(|_| anyhow::anyhow!("broker call lock failed"))?;
            if inner.stopped.load(Ordering::Relaxed) {
                return Err(problem("unavailable", "agent broker is closing"));
            }
            ensure!(
                !active.contains_key(&id),
                "broker request id already active"
            );
            if active.len() >= MAX_CALLS {
                return Err(problem("busy", "too many active agent broker calls"));
            }
            if let Some(owner) = api_owner.as_deref_mut() {
                owner.active_id = Some(id.clone());
                owner.cancel = Some(cancel.clone());
            }
            active.insert(id.clone(), cancel.clone());
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let running = inner.clone();
        let running_id = id.clone();
        let running_cancel = cancel.clone();
        let retained = api_owner.is_some();
        #[cfg(test)]
        let api_probe = if retained { inner.api_probe.lock().unwrap().take() } else { None };
        #[cfg(test)]
        let panic_request = api_probe.as_ref().is_some_and(|probe|probe.panic_request);
        let spawned = std::thread::Builder::new()
            .name("rudder-agent-api".into())
            .spawn(move || {
                #[cfg(test)]
                if let Some(probe) = api_probe { owned_thread_tests::install_api_probe(probe); }
                let result = match request.operation {
                    Operation::Call { tool, arguments } => {
                        snapshot
                            .api
                            .call_with_cancel(&tool, arguments, &running_cancel)
                    }
                    Operation::Pending => snapshot
                        .api
                        .pending_messages()
                        .and_then(|messages| Ok(serde_json::to_value(messages)?)),
                    _ => unreachable!(),
                };
                let _ = tx.send(result);
                if !retained {
                    if let Ok(mut active) = running.active.lock() {
                        active.remove(&running_id);
                    }
                }
            });
        match spawned {
            Ok(worker) => {
                if let Some(owner) = api_owner.as_deref_mut() { owner.worker = Some(worker); }
                else { drop(worker); } // Preserve ordinary legacy detached behavior.
            }
            Err(_) => {
                if !retained { inner.active.lock().ok().map(|mut active| active.remove(&id)); }
                return Err(problem("busy", "agent broker could not start a bounded call"));
            }
        }
        #[cfg(test)]
        if panic_request { panic!("injected owned request failure after actual API spawn"); }
        let deadline = Instant::now() + CALL_DEADLINE;
        loop {
            if inner.stopped.load(Ordering::Relaxed) || disconnected(&stream) {
                cancel.store(true, Ordering::Relaxed);
                return Err(problem(
                    "cancelled",
                    "agent broker call cancelled by connection shutdown",
                ));
            }
            if Instant::now() >= deadline {
                cancel.store(true, Ordering::Relaxed);
                return Err(problem(
                    "timeout",
                    "agent broker call deadline exceeded; inspect scoped receipts before retrying",
                ));
            }
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => {
                    return Err(problem(
                        "unavailable",
                        "agent broker call ended without confirmation",
                    ))
                }
            }
        }
    })();
    let response = match result {
        Ok(result) => Response {
            format_version: 1,
            id,
            result: Some(result),
            error: None,
        },
        Err(error) => Response {
            format_version: 1,
            id,
            result: None,
            error: Some(wire_error(&error)),
        },
    };
    let Ok(mut encoded) = serde_json::to_vec(&response) else {
        return;
    };
    if encoded.len() > MAX_RESPONSE {
        let fallback = Response {
            format_version: 1,
            id: response.id,
            result: None,
            error: Some(WireError {
                code: "response_too_large".into(),
                message: "agent broker response exceeded its bounded frame; request fewer items"
                    .into(),
            }),
        };
        let Ok(bytes) = serde_json::to_vec(&fallback) else {
            return;
        };
        encoded = bytes;
    }
    // Never append an error frame after a partial response write.
    let written = stream
        .write_all(&(encoded.len() as u32).to_be_bytes())
        .and_then(|_| stream.write_all(&encoded))
        .and_then(|_| stream.flush());
    #[cfg(test)]
    if let Err(error) = &written {
        eprintln!(
            "broker response write failed: bytes={} kind={:?} errno={:?}",
            encoded.len(),
            error.kind(),
            error.raw_os_error()
        );
    }
    let _ = written;
}

#[derive(Clone)]
pub(crate) struct BrokerClient {
    socket: PathBuf,
    identity: PathBuf,
    binding: BrokerBinding,
}

/// Nonblocking connect keeps a saturated Unix accept backlog from stalling the
/// helper's authorization watchdog. mio supplies the platform-specific socket
/// creation/readiness details, including close-on-exec on supported systems.
fn connect_until(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let mut stream = loop {
        if Instant::now() >= deadline { return Err(io::Error::new(io::ErrorKind::TimedOut, "broker connect deadline exceeded")); }
        match mio::net::UnixStream::connect(path) {
            Ok(stream) => break stream,
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => return Err(error),
        }
    };
    let mut poll = mio::Poll::new()?;
    poll.registry().register(&mut stream, mio::Token(0), mio::Interest::WRITABLE)?;
    let mut events = mio::Events::with_capacity(4);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() { return Err(io::Error::new(io::ErrorKind::TimedOut, "broker connect deadline exceeded")); }
        match poll.poll(&mut events, Some(remaining)) {
            Ok(()) if !events.is_empty() => break,
            Ok(()) => {},
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
            Err(error) => return Err(error),
        }
    }
    if let Some(error) = stream.take_error()? { return Err(error); }
    poll.registry().deregister(&mut stream)?;
    let stream = unsafe { UnixStream::from_raw_fd(stream.into_raw_fd()) };
    stream.set_nonblocking(false)?;
    Ok(stream)
}

impl BrokerClient {
    pub(crate) fn binding(&self) -> &BrokerBinding { &self.binding }

    pub(crate) fn connect_workspace(socket: PathBuf, identity: PathBuf, expected: &BrokerBinding, workspace: &rudder_engine::vfs::rpc::ObservationBinding) -> Result<Self> {
        let initial = read_identity(&identity)?;
        ensure!(BrokerBinding::from(&initial) == *expected, "workspace broker belongs to a replacement attempt");
        let client = Self { socket, identity, binding: expected.clone() };
        client.authorize_workspace(workspace)?;
        Ok(client)
    }

    pub(crate) fn authorize_workspace(&self, workspace: &rudder_engine::vfs::rpc::ObservationBinding) -> Result<()> {
        workspace.validate()?;
        let result = self.exchange(Operation::AuthorizeWorkspace { binding: workspace.clone() }, &AtomicBool::new(false))?;
        ensure!(serde_json::from_value::<BrokerBinding>(result["binding"].clone())? == self.binding && result["workspace_scope"] == rudder_engine::execution::workspace_scope(workspace)?, "workspace broker authorization binding differs");
        Ok(())
    }
    pub(crate) fn connect_partial_workspace(socket: PathBuf, identity: PathBuf, expected: &BrokerBinding, workspace: &rudder_engine::vfs::partial_rpc::PartialBinding) -> Result<Self> {
        let initial = read_identity(&identity)?;
        ensure!(BrokerBinding::from(&initial) == *expected, "partial broker belongs to a replacement attempt");
        let client = Self { socket, identity, binding: expected.clone() };
        client.authorize_partial_workspace(workspace)?;
        Ok(client)
    }
    pub(crate) fn authorize_partial_workspace(&self, workspace: &rudder_engine::vfs::partial_rpc::PartialBinding) -> Result<()> {
        workspace.validate()?;
        let result = self.exchange(Operation::AuthorizePartialWorkspace { binding: workspace.clone() }, &AtomicBool::new(false))?;
        ensure!(serde_json::from_value::<BrokerBinding>(result["binding"].clone())? == self.binding
            && result["partial_workspace_scope"] == rudder_engine::vfs::partial_rpc::partial_scope(workspace)?,
            "partial broker authorization binding differs");
        Ok(())
    }
    pub(crate) fn connect(socket: PathBuf, identity: PathBuf) -> Result<Self> {
        let initial = read_identity(&identity)?;
        let client = Self {
            socket,
            identity,
            binding: BrokerBinding::from(&initial),
        };
        let hello = client.exchange(Operation::Authenticate, &AtomicBool::new(false))?;
        ensure!(
            serde_json::from_value::<BrokerBinding>(hello["binding"].clone())? == client.binding,
            "agent broker handshake binding mismatch"
        );
        Ok(client)
    }
    fn identity(&self) -> Result<AgentIdentityFile> {
        let identity = read_identity(&self.identity)?;
        ensure!(
            BrokerBinding::from(&identity) == self.binding,
            "identity belongs to a replacement attempt; restart the adapter"
        );
        Ok(identity)
    }
    fn cancel(&self, id: &str) {
        let Ok(identity) = self.identity() else {
            return;
        };
        let Ok(mut stream) = UnixStream::connect(&self.socket) else {
            return;
        };
        let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
        let request = Request {
            format_version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            identity,
            operation: Operation::Cancel {
                request_id: id.into(),
            },
        };
        let _ = write_frame(&mut stream, &request, MAX_REQUEST);
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
    fn exchange(&self, operation: Operation, cancelled: &AtomicBool) -> Result<Value> {
        let workspace_auth = matches!(operation, Operation::AuthorizeWorkspace { .. } | Operation::AuthorizePartialWorkspace { .. });
        let deadline = Instant::now() + if workspace_auth { Duration::from_secs(2) } else { CONNECT_DEADLINE + CALL_DEADLINE + Duration::from_secs(1) };
        let offered = self.identity()?;
        let pending = matches!(operation, Operation::Pending);
        let result = self.exchange_once(operation.clone(), offered.clone(), cancelled, deadline);
        let rotation_window = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<WireError>())
            .is_some_and(|error| {
                error.code == "unauthorized" || ((pending || workspace_auth) && error.code == "rejected")
            });
        if rotation_window {
            // Revocation precedes atomic publication. Retry only pre-dispatch
            // authentication rejection (or a read-only channel poll), and only
            // after observing a different token in this SAME attempt.
            let rotation_deadline = deadline.min(Instant::now() + Duration::from_millis(500));
            while Instant::now() < rotation_deadline {
                ensure!(
                    !cancelled.load(Ordering::Relaxed),
                    "agent request cancelled"
                );
                let current = self.identity()?;
                if !token_equal(&offered.token, &current.token) {
                    return self.exchange_once(operation, current, cancelled, deadline);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        result
    }
    fn exchange_once(
        &self,
        operation: Operation,
        identity: AgentIdentityFile,
        cancelled: &AtomicBool,
        deadline: Instant,
    ) -> Result<Value> {
        let workspace_auth = matches!(operation, Operation::AuthorizeWorkspace { .. } | Operation::AuthorizePartialWorkspace { .. });
        let connect_deadline = deadline.min(Instant::now() + CONNECT_DEADLINE);
        let mut stream = loop {
            ensure!(
                !cancelled.load(Ordering::Relaxed),
                "agent request cancelled"
            );
            let connected = if workspace_auth { connect_until(&self.socket, connect_deadline) } else { UnixStream::connect(&self.socket) };
            match connected {
                Ok(stream) => break stream,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    if Instant::now() >= connect_deadline {
                        return Err(problem("unavailable","agent broker is unavailable; this worker cannot access authority files directly"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return Err(problem("unavailable", "agent broker connection failed")),
            }
        };
        stream.set_read_timeout(Some(Duration::from_millis(20)))?;
        ensure!(Instant::now() < deadline, "agent broker authorization deadline exceeded");
        stream.set_write_timeout(Some(Duration::from_secs(1).min(deadline.saturating_duration_since(Instant::now()))))?;
        let id = uuid::Uuid::new_v4().to_string();
        let request = Request {
            format_version: 1,
            id: id.clone(),
            identity,
            operation,
        };
        write_frame(&mut stream, &request, MAX_REQUEST).map_err(|error| {
            if error.downcast_ref::<io::Error>().is_some() {
                problem(
                    "unavailable",
                    "agent broker write ended without confirmation; inspect scoped receipts",
                )
            } else {
                error
            }
        })?;
        // The length prefix already delimits this request. Keep both halves
        // until its response: a fast server can close before a redundant
        // SHUT_WR, which Darwin reports as ENOTCONN despite a queued reply.
        let bytes = read_frame(
            &mut stream,
            MAX_RESPONSE,
            deadline,
            || {
                if cancelled.load(Ordering::Relaxed) {
                    self.cancel(&id);
                    return Err(problem("cancelled", "agent request cancelled"));
                }
                Ok(())
            },
        )?;
        let response: Response =
            serde_json::from_slice(&bytes).context("invalid agent broker response")?;
        ensure!(
            response.format_version == 1
                && response.id == id
                && response.result.is_some() != response.error.is_some(),
            "agent broker response binding mismatch"
        );
        if let Some(error) = response.error {
            return Err(error.into());
        }
        Ok(response.result.unwrap())
    }
    pub(crate) fn call_with_cancel(
        &self,
        tool: &str,
        arguments: Value,
        cancelled: &AtomicBool,
    ) -> Result<Value> {
        self.exchange(
            Operation::Call {
                tool: tool.into(),
                arguments,
            },
            cancelled,
        )
    }
    pub(crate) fn pending_messages(&self) -> Result<Vec<MessageEnvelope>> {
        Ok(serde_json::from_value(
            self.exchange(Operation::Pending, &AtomicBool::new(false))?,
        )?)
    }
}

/// Foreground host harness for qualification; production owns AgentBroker in
/// RuntimeState. No detached process or worker-visible journal argument exists.
pub(crate) fn run(args: &[String]) -> Result<()> {
    let mut journal = None;
    let mut identity = None;
    let mut socket = None;
    let mut i = 0;
    while i < args.len() {
        let key = &args[i];
        i += 1;
        let value = PathBuf::from(args.get(i).context("broker option requires a path")?);
        match key.as_str() {
            "--journal" => journal = Some(value),
            "--identity-file" => identity = Some(value),
            "--socket" => socket = Some(value),
            _ => bail!("usage: fabric broker --journal PATH --identity-file PATH --socket PATH"),
        };
        i += 1;
    }
    let broker = AgentBroker::spawn(BrokerConfig {
        journal_path: journal.context("--journal is required")?,
        identity_file: identity.context("--identity-file is required")?,
        socket_path: socket.context("--socket is required")?,
    })?;
    println!(
        "{}",
        json!({"format_version":1,"ready":true,"socket":broker.socket_path(),"binding":broker.binding()})
    );
    io::stdout().flush()?;
    let mut byte = [0_u8; 1];
    while io::stdin().read(&mut byte)? > 0 {}
    broker.close();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !broker.is_closed() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(test)]
#[path = "agent_broker_tests.rs"]
mod tests;

#[cfg(test)]
mod owned_thread_tests {
    use super::*;
    use std::cell::RefCell;
    pub(super) struct ApiProbe {
        tls: ApiTls,
        pub(super) panic_request: bool,
        panic_api: bool,
    }
    struct ApiTls {
        entered:mpsc::Sender<()>, release:mpsc::Receiver<()>,
        ended:Arc<AtomicBool>, expired:Arc<AtomicBool>,
    }
    impl Drop for ApiTls {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            if self.release.recv_timeout(Duration::from_secs(5)).is_err() {
                self.expired.store(true, Ordering::Release);
            }
            self.ended.store(true, Ordering::Release);
        }
    }
    thread_local! { static API_TLS: RefCell<Option<ApiTls>> = const { RefCell::new(None) }; }
    pub(super) fn install_api_probe(probe: ApiProbe) {
        let ApiProbe { tls, panic_api, .. } = probe;
        API_TLS.with(|slot| *slot.borrow_mut() = Some(tls));
        if panic_api { panic!("injected actual API-worker failure"); }
    }
    struct ApiRelease(Option<mpsc::Sender<()>>);
    impl ApiRelease { fn open(&mut self) { self.0.take().unwrap().send(()).unwrap(); } }
    impl Drop for ApiRelease {
        fn drop(&mut self) { if let Some(release) = self.0.take() { let _ = release.send(()); } }
    }
    struct RpcRoot(PathBuf);
    impl Drop for RpcRoot { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    fn actual_rpc_api_tls(panic_request:bool, panic_api:bool) {
        use rudder_engine::journal::{CapabilityGrant, Journal};
        use std::collections::BTreeSet;
        let root = RpcRoot(Path::new("/tmp").canonicalize().unwrap()
            .join(format!("rba-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])));
        private_directory(&root.0).unwrap();
        let journal_path = root.0.join("journal.sqlite");
        let mut journal = Journal::open(&journal_path).unwrap();
        let fence = journal.create_project("p", "owner", "base", 0).unwrap();
        journal.register_run(&fence, "run", 0).unwrap();
        let token = journal.issue_capability(&fence, &CapabilityGrant {
            principal:"agent".into(), run_id:"run".into(), run_epoch:1,
            operations:BTreeSet::from(["agents:find".into()]), recipients:BTreeMap::new(),
            scopes:BTreeSet::new(), topics:BTreeSet::new(), expires_at_ms:None,
        }, 0).unwrap();
        let socket = socket_path(&root.0, "p", "run", "attempt");
        let identity_file = project_identity(&socket, &AgentIdentityFile { format_version:1,
            project_id:"p".into(), run_id:"run".into(), run_epoch:1, owner_epoch:1, token }).unwrap();
        let mut owner = BrokerThreadOwner::new();
        let broker = AgentBroker::spawn_retaining(BrokerConfig { journal_path, identity_file:identity_file.clone(),
            socket_path:socket.clone() }, &mut owner).unwrap();
        let client = BrokerClient::connect(socket, identity_file).unwrap();
        let (entered, wait) = mpsc::channel(); let (release, gate) = mpsc::channel();
        let mut release = ApiRelease(Some(release));
        let ended = Arc::new(AtomicBool::new(false)); let expired = Arc::new(AtomicBool::new(false));
        *broker.inner.api_probe.lock().unwrap() = Some(ApiProbe { panic_request, panic_api,
            tls:ApiTls { entered, release:gate, ended:ended.clone(), expired:expired.clone() } });
        // Actual framed RPC, authentication and AgentApi query. A successful
        // reply is available before the API worker's TLS teardown completes.
        let result = client.exchange(Operation::Call { tool:"agents_find".into(), arguments:json!({}) }, &AtomicBool::new(false));
        if panic_request || panic_api { assert!(result.is_err()); }
        else { assert!(result.unwrap()["agents"].is_array()); }
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!ended.load(Ordering::Acquire));
        assert_eq!(broker.inner.active.lock().unwrap().len(), 1, "API TLS still owns its call permit");
        assert!(broker.inner.count.load(Ordering::Acquire) > 0);
        assert!(owner.await_closed(Duration::from_millis(20)).is_err());
        assert!(owner.joiner.is_some() && !owner.settled);
        let joining = owner.joiner.as_ref().unwrap()._thread.thread().id();
        assert!(!ended.load(Ordering::Acquire));
        assert!(!broker.inner.active.lock().unwrap().is_empty());
        release.open();
        let settled = owner.await_closed(Duration::from_secs(2));
        assert!(ended.load(Ordering::Acquire));
        assert!(!expired.load(Ordering::Acquire), "API TLS fixture fuse expired instead of release");
        assert_eq!(broker.inner.count.load(Ordering::Acquire), 0);
        assert!(broker.inner.active.lock().unwrap().is_empty());
        assert!(broker.inner.connections.lock().unwrap().is_empty());
        if panic_request || panic_api {
            assert!(settled.is_err() && !owner.settled);
            assert!(owner.calls.uncertain.load(Ordering::Acquire));
            assert_eq!(owner.calls.panics.lock().unwrap().len(), 1);
            assert_eq!(owner.joiner.as_ref().unwrap()._thread.thread().id(), joining);
            assert!(owner.await_closed(Duration::from_millis(1)).is_err());
        } else {
            let proof = settled.unwrap();
            assert_eq!(proof._joiner.as_ref().unwrap()._thread.thread().id(), joining);
            assert!(owner.settled && owner.calls.handles().is_empty());
            assert!(owner.calls.panics.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn owned_actual_rpc_reply_cannot_settle_before_api_worker_tls_join() {
        actual_rpc_api_tls(false, false);
    }
    #[test]
    fn owned_actual_rpc_handler_or_api_panic_retains_nested_tls_and_uncertainty() {
        actual_rpc_api_tls(true, false);
        actual_rpc_api_tls(false, true);
    }
    struct BlockTls { entered:mpsc::Sender<()>, release:mpsc::Receiver<()> }
    impl Drop for BlockTls {
        fn drop(&mut self) { let _ = self.entered.send(()); let _ = self.release.recv(); }
    }
    thread_local! { static TLS: RefCell<Option<BlockTls>> = const { RefCell::new(None) }; }
    #[test]
    fn owned_broker_retains_request_tls_join_on_timeout_and_retries_same_owner() {
        let (entered, wait) = mpsc::channel(); let (release, blocked) = mpsc::channel();
        let call = std::thread::spawn(move || TLS.with(|tls| *tls.borrow_mut() = Some(BlockTls { entered, release:blocked })));
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut owner = BrokerThreadOwner::new();
        owner.acceptor = Some(std::thread::spawn(|| {}));
        owner.calls.handles().push(call);
        assert!(owner.await_closed(Duration::from_millis(10)).is_err());
        assert!(owner.joiner.is_some()); assert!(!owner.settled);
        release.send(()).unwrap();
        let proof = owner.await_closed(Duration::from_secs(2)).unwrap();
        assert!(proof._joiner.is_some()); assert!(owner.settled);
        assert!(owner.await_closed(Duration::from_millis(1)).is_err());
    }

    #[test]
    fn owned_broker_sequential_requests_exceed_old_lifetime_cap_with_bounded_handles() {
        use rudder_engine::journal::{CapabilityGrant, Journal};
        use std::collections::{BTreeMap, BTreeSet};
        let root = Path::new("/tmp").canonicalize().unwrap()
            .join(format!("rbc-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
        private_directory(&root).unwrap();
        let journal_path = root.join("journal.sqlite");
        let mut journal = Journal::open(&journal_path).unwrap();
        let fence = journal.create_project("p", "owner", "base", 0).unwrap();
        journal.register_run(&fence, "run", 0).unwrap();
        let token = journal.issue_capability(&fence, &CapabilityGrant {
            principal:"agent".into(), run_id:"run".into(), run_epoch:1,
            operations:BTreeSet::from(["agents:find".into()]), recipients:BTreeMap::new(),
            scopes:BTreeSet::new(), topics:BTreeSet::new(), expires_at_ms:None,
        }, 0).unwrap();
        let socket = socket_path(&root, "p", "run", "attempt");
        project_identity(&socket, &AgentIdentityFile { format_version:1, project_id:"p".into(),
            run_id:"run".into(), run_epoch:1, owner_epoch:1, token }).unwrap();
        let mut owner = BrokerThreadOwner::new();
        let broker = AgentBroker::spawn_retaining(BrokerConfig { journal_path,
            identity_file:socket.with_file_name("identity.json"), socket_path:socket.clone() }, &mut owner).unwrap();
        let client = BrokerClient::connect(socket.clone(), socket.with_file_name("identity.json")).unwrap();
        for _ in 0..1030 {
            client.exchange(Operation::Authenticate, &AtomicBool::new(false)).unwrap();
            let handles = owner.calls.handles();
            assert!(handles.len() <= MAX_OWNED_UNJOINED);
            assert!(handles.capacity() <= MAX_OWNED_UNJOINED);
            assert!(!broker.is_closed());
        }
        assert!(broker.inner.sequence.load(Ordering::Acquire) > 1024);
        let proof = owner.await_closed(Duration::from_secs(2)).unwrap();
        assert!(proof._joiner.is_some());
        drop(proof); drop(client); drop(broker); drop(owner); drop(journal);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn owned_broker_blocked_tls_keeps_unjoined_capacity_until_actual_join() {
        let mut owner = BrokerThreadOwner::new();
        let mut releases = Vec::new();
        for _ in 0..MAX_OWNED_UNJOINED {
            let (entered, wait) = mpsc::channel(); let (release, blocked) = mpsc::channel();
            let call = std::thread::spawn(move || TLS.with(|tls|
                *tls.borrow_mut() = Some(BlockTls { entered, release:blocked })));
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            owner.calls.track(call); releases.push(release);
        }
        let calls = Arc::clone(&owner.calls);
        let (polled, poll) = mpsc::channel();
        owner.acceptor = Some(std::thread::spawn(move || {
            let _ = polled.send(calls.reap_finished());
        }));
        // A platform may report no finished candidate, or its finished hint
        // may lead the actual join into blocked TLS. Neither permits admission.
        match poll.recv_timeout(Duration::from_millis(10)) {
            Ok(result) => assert!(!result.unwrap()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => panic!("owned reaper exited without its result"),
        }
        assert!(owner.calls.handles().len() <= MAX_OWNED_UNJOINED);
        assert!(owner.await_closed(Duration::from_millis(10)).is_err());
        assert!(!owner.settled);
        for release in releases { release.send(()).unwrap(); }
        owner.await_closed(Duration::from_secs(2)).unwrap();
        assert!(owner.settled);
    }

    #[test]
    fn owned_broker_reaped_panic_stays_uncertain_after_handle_vector_is_empty() {
        let mut owner = BrokerThreadOwner::new();
        let call = std::thread::spawn(|| panic!("injected broker handler panic"));
        let end = Instant::now() + Duration::from_secs(2);
        while !call.is_finished() && Instant::now() < end { std::thread::yield_now(); }
        assert!(call.is_finished());
        owner.calls.track(call);
        assert!(owner.calls.reap_finished().is_err());
        assert!(owner.calls.handles().is_empty());
        assert!(owner.calls.reap_finished().is_err());
        owner.acceptor = Some(std::thread::spawn(|| {}));
        assert!(owner.await_closed(Duration::from_secs(2)).is_err());
        assert!(!owner.settled);
        assert_eq!(owner.calls.panics.lock().unwrap().len(), 1);
    }

    #[test]
    fn owned_broker_poison_stays_uncertain_even_if_mutex_is_later_cleared() {
        let mut owner = BrokerThreadOwner::new();
        let calls = Arc::clone(&owner.calls);
        assert!(std::thread::spawn(move || {
            let _held = calls.handles.lock().unwrap(); panic!("injected tracked handles poison");
        }).join().is_err());
        assert!(owner.calls.reap_finished().is_err());
        owner.calls.handles.clear_poison();
        assert!(owner.calls.handles().is_empty());
        assert!(owner.calls.reap_finished().is_err());
        assert!(owner.await_closed(Duration::from_millis(10)).is_err());
        assert!(!owner.settled);
    }
}
