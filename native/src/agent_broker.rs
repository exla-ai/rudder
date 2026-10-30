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
use std::os::fd::AsRawFd;
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
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
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
        });
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
        std::thread::Builder::new()
            .name("rudder-agent-broker".into())
            .spawn(move || {
                let _ownership = ownership;
                while !worker.stopped.load(Ordering::Relaxed) {
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
                            if spawned.is_err() {
                                if let Ok(mut connections) = worker.connections.lock() {
                                    connections.remove(&id);
                                }
                                worker.count.fetch_sub(1, Ordering::Relaxed);
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
        self.close();
    }
}

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
fn disconnected(stream: &UnixStream) -> bool {
    let mut fd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    (unsafe { libc::poll(&mut fd, 1, 0) }) >= 0
        && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}
fn connection_call(inner: &Arc<Inner>, mut stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
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
            active.insert(id.clone(), cancel.clone());
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let running = inner.clone();
        let running_id = id.clone();
        let running_cancel = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("rudder-agent-api".into())
            .spawn(move || {
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
                if let Ok(mut active) = running.active.lock() {
                    active.remove(&running_id);
                }
            });
        if spawned.is_err() {
            inner
                .active
                .lock()
                .ok()
                .map(|mut active| active.remove(&id));
            return Err(problem(
                "busy",
                "agent broker could not start a bounded call",
            ));
        }
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
    let _ = stream
        .write_all(&(encoded.len() as u32).to_be_bytes())
        .and_then(|_| stream.write_all(&encoded))
        .and_then(|_| stream.flush());
}

#[derive(Clone)]
pub(crate) struct BrokerClient {
    socket: PathBuf,
    identity: PathBuf,
    binding: BrokerBinding,
}
impl BrokerClient {
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
        let offered = self.identity()?;
        let pending = matches!(operation, Operation::Pending);
        let result = self.exchange_once(operation.clone(), offered.clone(), cancelled);
        let rotation_window = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<WireError>())
            .is_some_and(|error| {
                error.code == "unauthorized" || (pending && error.code == "rejected")
            });
        if rotation_window {
            // Revocation precedes atomic publication. Retry only pre-dispatch
            // authentication rejection (or a read-only channel poll), and only
            // after observing a different token in this SAME attempt.
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                ensure!(
                    !cancelled.load(Ordering::Relaxed),
                    "agent request cancelled"
                );
                let current = self.identity()?;
                if !token_equal(&offered.token, &current.token) {
                    return self.exchange_once(operation, current, cancelled);
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
    ) -> Result<Value> {
        let connect_deadline = Instant::now() + CONNECT_DEADLINE;
        let mut stream = loop {
            ensure!(
                !cancelled.load(Ordering::Relaxed),
                "agent request cancelled"
            );
            match UnixStream::connect(&self.socket) {
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
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
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
        // A request's write half may close normally; only explicit cancellation
        // or a complete connection hangup cancels its bounded API operation.
        stream.shutdown(std::net::Shutdown::Write).map_err(|_| {
            problem(
                "unavailable",
                "agent broker connection ended without confirmation",
            )
        })?;
        let bytes = read_frame(
            &mut stream,
            MAX_RESPONSE,
            Instant::now() + CALL_DEADLINE + Duration::from_secs(1),
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
