//! One persistent, private Node transport for a fixed trusted Cloud scope.
//! The engine supplies auth once over stdio, never argv/env/disk/CAS. This
//! source fetches bounded objects and existing replicated root receipts; neither
//! operation grants root membership or creates a complete LocalStore pin.
//!
//! Cancellation retains native admission until the helper's terminal response
//! proves HTTP settlement, or the owned child is actually reaped. A hung helper
//! never triggers automatic replacement. Scoped process slots and fixed I/O
//! threads remain owned even after startup failure until the child/IO exits.
//!
//! One live scope owns one child and four fixed native threads (reaper, stdin,
//! stdout, stderr); at most eight scopes exist in this process. Byte admission
//! charges declared encoded payloads, not whole-process RSS: native snapshots,
//! the cache sink and Node's bounded response buffers are separate allocations.
//! The helper permits at most eight jobs, 64MiB declared payloads, 64KiB frames,
//! 16KiB response headers and eight HTTP sockets. Legacy >4MiB blobs/metadata
//! >64KiB require an explicit bulk path. Cache hits never enter this source.
//!
//! Authentication refresh is fail-closed: abort/drain/drop old owners before
//! creating a new helper for the same scope. HTTPS uses standard certificate
//! checks; pinned startup DNS requires an explicit restart when addresses change.
//! Linux parent death is tied to the persistent reaper thread. No new complete
//! local root, root membership proof, guest credential or VFS default is created.

use crate::protocol::{ObjectKind, ObjectRef};
use crate::remote_cache::{
    CacheScope, FetchControl, ImmutableObjectSource, MAX_BLOB_BYTES, MAX_METADATA_BYTES,
    TRANSFER_BUFFER_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{
    mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    Arc, Condvar, Mutex, OnceLock, Weak,
};
use std::time::{Duration, Instant};
use uuid::Uuid;

const MAX_FRAME: usize = 64 * 1024;
const MAX_HELPERS: usize = 8;
const STDERR_LIMIT: u64 = 64 * 1024;
const INIT: u8 = 1;
const OBJECT: u8 = 2;
const ROOT: u8 = 3;
const CANCEL: u8 = 4;
const SHUTDOWN: u8 = 5;
const HELLO: u8 = 10;
const DATA: u8 = 11;
const OBJECT_DONE: u8 = 12;
const ROOT_DONE: u8 = 13;
const ERROR: u8 = 14;
const FATAL: u8 = 15;
const SHUTDOWN_ACK: u8 = 16;
type Result<T> = std::result::Result<T, SourceError>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceError {
    Busy,
    InvalidConfig,
    Protocol,
    TimedOut,
    Cancelled,
    Closed,
    RemoteFailed,
    ChildExited,
    Io,
}
impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "Cloud source admission is busy",
            Self::InvalidConfig => "invalid private Cloud source configuration",
            Self::Protocol => "private Cloud transport protocol failed",
            Self::TimedOut => "Cloud object request timed out",
            Self::Cancelled => "Cloud object request cancelled",
            Self::Closed => "Cloud source is closed",
            Self::RemoteFailed => "Cloud object request failed",
            Self::ChildExited => "Cloud transport exited",
            Self::Io => "private Cloud transport I/O failed",
        })
    }
}
impl std::error::Error for SourceError {}
impl SourceError {
    pub fn errno(self) -> i32 {
        match self {
            Self::Busy => libc::EAGAIN,
            Self::TimedOut => libc::ETIMEDOUT,
            Self::Cancelled | Self::Closed => libc::ECANCELED,
            Self::InvalidConfig => libc::EINVAL,
            _ => libc::EIO,
        }
    }
}
fn locked<T>(value: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    value.lock().map_err(|_| SourceError::Protocol)
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportProfile {
    Https,
    LoopbackHttpTest,
}
/// Deliberately not Debug/Serialize: headers belong only in the private init.
pub struct NodeSourceConfig {
    pub scope: CacheScope,
    pub base_url: String,
    pub auth_headers: BTreeMap<String, String>,
    pub transport: TransportProfile,
    pub max_jobs: usize,
    /// Encoded payload reservation. Does not claim to cap whole-process RSS.
    pub max_inflight_bytes: u64,
    pub http_timeout: Duration,
    pub handshake_timeout: Duration,
}
impl NodeSourceConfig {
    pub fn new(
        scope: CacheScope,
        base_url: String,
        auth_headers: BTreeMap<String, String>,
    ) -> Self {
        Self {
            scope,
            base_url,
            auth_headers,
            transport: TransportProfile::Https,
            max_jobs: 8,
            max_inflight_bytes: 16 * 1024 * 1024,
            http_timeout: Duration::from_secs(30),
            handshake_timeout: Duration::from_secs(5),
        }
    }
    fn validate(&self) -> Result<()> {
        self.scope
            .digest()
            .map_err(|_| SourceError::InvalidConfig)?;
        if self.max_jobs == 0
            || self.max_jobs > 8
            || self.max_inflight_bytes == 0
            || self.max_inflight_bytes > 64 * 1024 * 1024
            || self.http_timeout.as_millis() == 0
            || self.http_timeout > Duration::from_secs(120)
            || self.handshake_timeout.as_millis() == 0
            || self.handshake_timeout > Duration::from_secs(30)
            || self.base_url.len() > 2048
            || self.base_url.trim() != self.base_url
            || self.base_url.contains(['?', '#', '\\'])
        {
            return Err(SourceError::InvalidConfig);
        }
        let authority = match self.transport {
            TransportProfile::Https => self.base_url.strip_prefix("https://"),
            TransportProfile::LoopbackHttpTest => self.base_url.strip_prefix("http://"),
        }
        .ok_or(SourceError::InvalidConfig)?
        .split('/')
        .next()
        .ok_or(SourceError::InvalidConfig)?;
        if authority.is_empty()
            || authority.contains('@')
            || authority
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(SourceError::InvalidConfig);
        }
        if matches!(self.transport, TransportProfile::LoopbackHttpTest) {
            let host = authority
                .strip_prefix("127.0.0.1")
                .or_else(|| authority.strip_prefix("[::1]"))
                .ok_or(SourceError::InvalidConfig)?;
            if !host.is_empty()
                && (!host.starts_with(':')
                    || host.len() == 1
                    || !host[1..].bytes().all(|b| b.is_ascii_digit()))
            {
                return Err(SourceError::InvalidConfig);
            }
        }
        let mut names = BTreeMap::new();
        for (name, value) in &self.auth_headers {
            let lower = name.to_ascii_lowercase();
            if !["authorization", "x-rudder-workspace-id"].contains(&lower.as_str())
                || names.insert(lower.clone(), ()).is_some()
                || value.is_empty()
                || value.len() > 8192
                || value.contains(['\r', '\n', '\0'])
            {
                return Err(SourceError::InvalidConfig);
            }
        }
        if !names.contains_key("authorization") {
            return Err(SourceError::InvalidConfig);
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct SourceRequestControl {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}
impl SourceRequestControl {
    pub fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(SourceError::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(SourceError::TimedOut)
        } else {
            Ok(())
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicatedRootReceipt {
    pub format_version: u32,
    pub root_id: String,
    pub root: ObjectRef,
    pub durability: ReplicatedDurability,
    pub published_at_ms: u64,
    pub object_count: u64,
    pub total_bytes: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicatedDurability {
    Replicated,
}
impl ReplicatedRootReceipt {
    fn validate(&self, expected: &str) -> Result<()> {
        if self.format_version != 1
            || self.root_id != expected
            || self.published_at_ms > 9_007_199_254_740_991
            || self.object_count == 0
            || self.object_count > 100_000
            || self.total_bytes > 4 * 1024 * 1024 * 1024
        {
            return Err(SourceError::Protocol);
        }
        general_reference(&self.root)
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct SourceStats {
    pub pending_jobs: usize,
    pub pending_bytes: u64,
    pub stderr_bytes_suppressed: u64,
    pub child_exited: bool,
    pub closing: bool,
}
#[derive(Serialize)]
struct Init<'a> {
    format_version: u32,
    nonce: &'a str,
    scope: &'a CacheScope,
    base_url: &'a str,
    auth_headers: &'a BTreeMap<String, String>,
    transport: TransportProfile,
    max_jobs: usize,
    max_inflight_bytes: u64,
    http_timeout_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    format_version: u32,
    nonce: String,
    scope: CacheScope,
    max_jobs: usize,
    max_inflight_bytes: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireError {
    code: String,
}
struct Frame {
    kind: u8,
    id: u64,
    body: Vec<u8>,
}
impl Frame {
    fn encode(self) -> Result<Vec<u8>> {
        if self.body.len() > MAX_FRAME {
            return Err(SourceError::Protocol);
        }
        let mut bytes = vec![0; 16];
        bytes[0] = self.kind;
        bytes[4..12].copy_from_slice(&self.id.to_be_bytes());
        bytes[12..16].copy_from_slice(&(self.body.len() as u32).to_be_bytes());
        bytes.extend(self.body);
        Ok(bytes)
    }
}
enum Expected {
    Object(ObjectRef),
    Root(String),
}
enum Response {
    Object(Vec<u8>),
    Root(ReplicatedRootReceipt),
}
struct Pending {
    expected: Expected,
    buffer: Vec<u8>,
    result: Option<Result<Response>>,
    cost: u64,
}
struct State {
    hello: bool,
    next_id: u64,
    pending: BTreeMap<u64, Pending>,
    bytes: u64,
    closing: bool,
    exited: bool,
    failure: Option<SourceError>,
}
struct Shared {
    scope: CacheScope,
    nonce: String,
    max_jobs: usize,
    max_bytes: u64,
    http_timeout: Duration,
    outbound: SyncSender<Frame>,
    state: Mutex<State>,
    changed: Condvar,
    kill: AtomicBool,
    stderr_bytes: AtomicU64,
}
struct Owner {
    shared: Arc<Shared>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.shared.fail(SourceError::Closed);
    }
}
#[derive(Clone)]
pub struct NodeRemoteSource(Arc<Owner>);
impl std::fmt::Debug for NodeRemoteSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeRemoteSource").finish_non_exhaustive()
    }
}
static SOURCES: OnceLock<Mutex<BTreeMap<String, Weak<Shared>>>> = OnceLock::new();

impl NodeRemoteSource {
    /// Blocking startup outside actors. The fixed argv is only script+internal
    /// command. Env is cleared; private init carries all endpoint/auth settings.
    pub fn spawn(
        node: impl AsRef<Path>,
        cli_script: impl AsRef<Path>,
        config: NodeSourceConfig,
    ) -> Result<Self> {
        config.validate()?;
        if !node.as_ref().is_absolute() || !cli_script.as_ref().is_absolute() {
            return Err(SourceError::InvalidConfig);
        }
        let nonce = Uuid::new_v4().to_string();
        let init = serde_json::to_vec(&Init {
            format_version: 1,
            nonce: &nonce,
            scope: &config.scope,
            base_url: &config.base_url,
            auth_headers: &config.auth_headers,
            transport: config.transport,
            max_jobs: config.max_jobs,
            max_inflight_bytes: config.max_inflight_bytes,
            http_timeout_ms: config.http_timeout.as_millis() as u64,
        })
        .map_err(|_| SourceError::InvalidConfig)?;
        if init.len() > MAX_FRAME {
            return Err(SourceError::InvalidConfig);
        }
        let (tx, rx) = mpsc::sync_channel(2 * config.max_jobs + 4);
        let shared = Arc::new(Shared {
            scope: config.scope.clone(),
            nonce,
            max_jobs: config.max_jobs,
            max_bytes: config.max_inflight_bytes,
            http_timeout: config.http_timeout,
            outbound: tx,
            state: Mutex::new(State {
                hello: false,
                next_id: 1,
                pending: BTreeMap::new(),
                bytes: 0,
                closing: false,
                exited: false,
                failure: None,
            }),
            changed: Condvar::new(),
            kill: AtomicBool::new(false),
            stderr_bytes: AtomicU64::new(0),
        });
        {
            let mut sources = locked(SOURCES.get_or_init(|| Mutex::new(BTreeMap::new())))?;
            // Even a reaped child may have a stuck inherited pipe reader, or
            // a caller may still own its source. Never multiply those fixed
            // resources by repeatedly replacing a failed helper.
            sources.retain(|_, source| source.strong_count() > 0);
            let key = config
                .scope
                .digest()
                .map_err(|_| SourceError::InvalidConfig)?;
            if sources.contains_key(&key) || sources.len() >= MAX_HELPERS {
                return Err(SourceError::Busy);
            }
            sources.insert(key, Arc::downgrade(&shared));
        }
        let mut command = Command::new(node.as_ref());
        command
            .arg(cli_script.as_ref())
            .arg("__fabric-fetch")
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        #[cfg(target_os = "linux")]
        {
            let parent = std::process::id() as libc::pid_t;
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                        || libc::getppid() != parent
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let handshake_end = Instant::now() + config.handshake_timeout;
        start_reaper(shared.clone(), command, startup_tx)?;
        let (stdin, stdout, stderr) = match startup_rx
            .recv_timeout(handshake_end.saturating_duration_since(Instant::now()))
        {
            Ok(Ok(pipes)) => pipes,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                shared.fail(SourceError::TimedOut);
                return Err(SourceError::TimedOut);
            }
        };
        let write_shared = shared.clone();
        if std::thread::Builder::new()
            .name("rudder-fetch-write".into())
            .spawn(move || write_loop(write_shared, rx, stdin))
            .is_err()
        {
            shared.fail(SourceError::Io);
            return Err(SourceError::Io);
        }
        let read_shared = shared.clone();
        if std::thread::Builder::new()
            .name("rudder-fetch-read".into())
            .spawn(move || read_loop(read_shared, stdout))
            .is_err()
        {
            shared.fail(SourceError::Io);
            return Err(SourceError::Io);
        }
        let error_shared = shared.clone();
        if std::thread::Builder::new()
            .name("rudder-fetch-stderr".into())
            .spawn(move || stderr_loop(error_shared, stderr))
            .is_err()
        {
            shared.fail(SourceError::Io);
            return Err(SourceError::Io);
        }
        shared.send(Frame {
            kind: INIT,
            id: 0,
            body: init,
        })?;
        let end = handshake_end;
        let mut state = locked(&shared.state)?;
        loop {
            if state.exited {
                return Err(state.failure.unwrap_or(SourceError::ChildExited));
            }
            if state.hello && !state.closing {
                drop(state);
                return Ok(Self(Arc::new(Owner { shared })));
            }
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(state);
                shared.fail(SourceError::TimedOut);
                return Err(SourceError::TimedOut);
            }
            state = shared
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| SourceError::Protocol)?
                .0;
        }
    }
    pub fn stats(&self) -> Result<SourceStats> {
        let state = locked(&self.0.shared.state)?;
        Ok(SourceStats {
            pending_jobs: state.pending.len(),
            pending_bytes: state.bytes,
            stderr_bytes_suppressed: self.0.shared.stderr_bytes.load(Ordering::Acquire),
            child_exited: state.exited,
            closing: state.closing,
        })
    }
    /// Nonblocking request to abort/drain. No new jobs are admitted.
    pub fn shutdown(&self) {
        let shared = &self.0.shared;
        let send = if let Ok(mut state) = shared.state.lock() {
            if state.closing {
                false
            } else {
                state.closing = true;
                true
            }
        } else {
            false
        };
        if send {
            let _ = shared.send(Frame {
                kind: SHUTDOWN,
                id: 0,
                body: Vec::new(),
            });
        }
    }
    /// Explicit owned-child termination. Job errors are released only on reap.
    pub fn terminate(&self) {
        self.0.shared.fail(SourceError::Closed);
    }
    /// Blocking read-only lookup, called off-actor. Deadline/cancellation sends
    /// abort then waits for settlement; a hung helper retains admission.
    pub fn get_root(
        &self,
        root_id: &str,
        control: &SourceRequestControl,
    ) -> Result<ReplicatedRootReceipt> {
        if !root_id_ok(root_id) {
            return Err(SourceError::InvalidConfig);
        }
        control.check()?;
        let body=serde_json::to_vec(&serde_json::json!({"root_id":root_id,"timeout_ms":timeout_ms(control.deadline,self.0.shared.http_timeout)})).map_err(|_|SourceError::Protocol)?;
        let id = self
            .0
            .shared
            .submit(Expected::Root(root_id.into()), ROOT, body, 64 * 1024)?;
        let result = self.0.shared.await_response(id, || control.check());
        self.0.shared.release(id);
        match result? {
            Response::Root(receipt) => Ok(receipt),
            _ => Err(SourceError::Protocol),
        }
    }
}
impl ImmutableObjectSource for NodeRemoteSource {
    fn scope(&self) -> CacheScope {
        self.0.shared.scope.clone()
    }
    fn fetch(
        &self,
        reference: &ObjectRef,
        control: &FetchControl,
        output: &mut dyn Write,
    ) -> anyhow::Result<()> {
        self.fetch_object(reference, control, output)
            .map_err(|error| {
                // Only these concrete trusted control outcomes cross the cache's
                // sanitizer. Provider text/protocol/auth failures stay opaque.
                match error.downcast_ref::<SourceError>() {
                    Some(SourceError::Busy) => crate::remote_cache::CacheError::Busy.into(),
                    Some(SourceError::TimedOut) => crate::remote_cache::CacheError::TimedOut.into(),
                    Some(SourceError::Cancelled) => {
                        crate::remote_cache::CacheError::Cancelled.into()
                    }
                    _ => error,
                }
            })
    }
}
impl NodeRemoteSource {
    fn fetch_object(
        &self,
        reference: &ObjectRef,
        control: &FetchControl,
        output: &mut dyn Write,
    ) -> anyhow::Result<()> {
        partial_reference(reference)?;
        control.check()?;
        let body = serde_json::to_vec(
            &serde_json::json!({"reference":reference,"timeout_ms":timeout_ms(control.deadline(),self.0.shared.http_timeout)}),
        )?;
        let id = self.0.shared.submit(
            Expected::Object(reference.clone()),
            OBJECT,
            body,
            reference.size_bytes.max(1),
        )?;
        let result = (|| -> anyhow::Result<()> {
            let response = self.0.shared.await_response(id, || {
                control.check().map_err(|error| {
                    if error == crate::remote_cache::CacheError::TimedOut {
                        SourceError::TimedOut
                    } else {
                        SourceError::Cancelled
                    }
                })
            })?;
            control.check()?;
            let Response::Object(bytes) = response else {
                return Err(SourceError::Protocol.into());
            };
            reference
                .verify(&bytes)
                .map_err(|_| SourceError::Protocol)?;
            crate::store::metadata_references(reference.kind, &bytes)
                .map_err(|_| SourceError::Protocol)?;
            for chunk in bytes.chunks(TRANSFER_BUFFER_BYTES) {
                control.check()?;
                if locked(&self.0.shared.state)?.exited {
                    return Err(SourceError::ChildExited.into());
                }
                output.write_all(chunk)?;
            }
            Ok(())
        })();
        self.0.shared.release(id);
        result
    }
}
fn timeout_ms(deadline: Instant, maximum: Duration) -> u64 {
    deadline
        .saturating_duration_since(Instant::now())
        .min(maximum)
        .as_millis()
        .max(1)
        .min(120_000) as u64
}
fn root_id_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn general_reference(reference: &ObjectRef) -> Result<()> {
    reference.validate().map_err(|_| SourceError::Protocol)?;
    let cap = match reference.kind {
        ObjectKind::Blob => 256 * 1024 * 1024,
        ObjectKind::File | ObjectKind::DirectoryShard | ObjectKind::RunnableRoot => 64 * 1024,
        _ => 4 * 1024 * 1024,
    };
    if reference.size_bytes > cap {
        return Err(SourceError::Protocol);
    }
    Ok(())
}
fn partial_reference(reference: &ObjectRef) -> Result<()> {
    general_reference(reference)?;
    let cap = if reference.kind == ObjectKind::Blob {
        MAX_BLOB_BYTES
    } else {
        MAX_METADATA_BYTES
    };
    if reference.size_bytes > cap {
        return Err(SourceError::InvalidConfig);
    }
    Ok(())
}
impl Shared {
    fn send(&self, frame: Frame) -> Result<()> {
        self.outbound.try_send(frame).map_err(|_| {
            self.fail(SourceError::Io);
            SourceError::Io
        })
    }
    fn fail(&self, error: SourceError) {
        if let Ok(mut state) = self.state.lock() {
            state.closing = true;
            state.failure.get_or_insert(error);
        }
        self.kill.store(true, Ordering::Release);
        self.changed.notify_all();
    }
    fn submit(&self, expected: Expected, kind: u8, body: Vec<u8>, cost: u64) -> Result<u64> {
        let mut state = locked(&self.state)?;
        if state.closing || state.exited {
            return Err(SourceError::Closed);
        }
        if !state.hello {
            return Err(SourceError::Protocol);
        }
        let bytes = state.bytes.checked_add(cost).ok_or(SourceError::Busy)?;
        if state.pending.len() >= self.max_jobs || bytes > self.max_bytes {
            return Err(SourceError::Busy);
        }
        let id = state.next_id;
        state.next_id = state.next_id.checked_add(1).ok_or(SourceError::Busy)?;
        state.bytes = bytes;
        let capacity = if matches!(&expected, Expected::Object(_)) {
            cost as usize
        } else {
            0
        };
        state.pending.insert(
            id,
            Pending {
                expected,
                buffer: Vec::with_capacity(capacity),
                result: None,
                cost,
            },
        );
        // ID allocation and request enqueue share one serialization point.
        // Otherwise concurrent source workers could send #2 before #1, which
        // correctly fails the helper's monotonic replay fence.
        if self.outbound.try_send(Frame { kind, id, body }).is_err() {
            state.closing = true;
            state.failure.get_or_insert(SourceError::Io);
            self.kill.store(true, Ordering::Release);
            self.changed.notify_all();
        }
        Ok(id)
    }
    fn await_response(&self, id: u64, mut check: impl FnMut() -> Result<()>) -> Result<Response> {
        let mut cancellation = None;
        loop {
            if cancellation.is_none() {
                if let Err(error) = check() {
                    cancellation = Some(error);
                    let _ = self.send(Frame {
                        kind: CANCEL,
                        id,
                        body: Vec::new(),
                    });
                }
            }
            let mut state = locked(&self.state)?;
            if state.exited {
                return Err(cancellation
                    .or(state.failure)
                    .unwrap_or(SourceError::ChildExited));
            }
            let pending = state.pending.get_mut(&id).ok_or(SourceError::Protocol)?;
            if let Some(result) = pending.result.take() {
                if let Some(error) = cancellation {
                    return Err(error);
                }
                return result;
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(5))
                .map_err(|_| SourceError::Protocol)?
                .0;
            drop(state);
        }
    }
    fn release(&self, id: u64) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(pending) = state.pending.remove(&id) {
                state.bytes = state.bytes.saturating_sub(pending.cost);
            }
        }
    }
}
fn mark_exited(shared: &Shared) {
    if let Ok(mut state) = shared.state.lock() {
        state.exited = true;
        state.closing = true;
        state.failure.get_or_insert(SourceError::ChildExited);
    }
    shared.changed.notify_all();
}
fn start_reaper(
    shared: Arc<Shared>,
    mut command: Command,
    startup: SyncSender<Result<(ChildStdin, ChildStdout, ChildStderr)>>,
) -> Result<()> {
    let worker = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("rudder-fetch-reap".into())
        .spawn(move || {
            // Linux PDEATHSIG follows the thread that forked, not just the
            // process. This persistent thread MUST create and reap the child;
            // a short-lived caller/preparation thread may return immediately.
            let shared = worker;
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(_) => {
                    shared.fail(SourceError::Io);
                    mark_exited(&shared);
                    let _ = startup.send(Err(SourceError::Io));
                    return;
                }
            };
            let pipes = match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(input), Some(output), Some(error)) => Ok((input, output, error)),
                _ => Err(SourceError::Io),
            };
            let invalid = pipes.is_err();
            if startup.send(pipes).is_err() || invalid {
                shared.fail(SourceError::Io);
            }
            loop {
                if shared.kill.load(Ordering::Acquire) {
                    // The child PID has not been reaped, so this owned process
                    // group cannot have been recycled into an unrelated PID.
                    unsafe {
                        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                    }
                    let _ = child.kill();
                }
                match child.try_wait() {
                    Ok(Some(_)) => {
                        mark_exited(&shared);
                        return;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        shared.fail(SourceError::Io);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
    if spawned.is_err() {
        // No process or I/O thread has been created in this case.
        shared.fail(SourceError::Io);
        mark_exited(&shared);
        return Err(SourceError::Io);
    }
    Ok(())
}
fn write_loop(shared: Arc<Shared>, receiver: Receiver<Frame>, mut writer: impl Write) {
    loop {
        if shared
            .state
            .lock()
            .map(|state| state.exited)
            .unwrap_or(true)
        {
            return;
        }
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(frame) => {
                let result = frame
                    .encode()
                    .and_then(|bytes| writer.write_all(&bytes).map_err(|_| SourceError::Io));
                if let Err(error) = result {
                    shared.fail(error);
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                shared.fail(SourceError::Closed);
                return;
            }
        }
    }
}
fn read_frame(reader: &mut impl Read) -> Result<Frame> {
    let mut header = [0; 16];
    reader
        .read_exact(&mut header)
        .map_err(|_| SourceError::Io)?;
    if header[1..4] != [0, 0, 0] {
        return Err(SourceError::Protocol);
    }
    let length = u32::from_be_bytes(header[12..16].try_into().unwrap()) as usize;
    if length > MAX_FRAME {
        return Err(SourceError::Protocol);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|_| SourceError::Io)?;
    Ok(Frame {
        kind: header[0],
        id: u64::from_be_bytes(header[4..12].try_into().unwrap()),
        body,
    })
}
fn read_loop(shared: Arc<Shared>, mut reader: impl Read) {
    loop {
        let frame = match read_frame(&mut reader) {
            Ok(frame) => frame,
            Err(error) => {
                shared.fail(error);
                return;
            }
        };
        let result = (|| -> Result<()> {
            let mut state = locked(&shared.state)?;
            if frame.kind == HELLO {
                if frame.id != 0 || state.hello || state.closing {
                    return Err(SourceError::Protocol);
                }
                let hello: Hello =
                    serde_json::from_slice(&frame.body).map_err(|_| SourceError::Protocol)?;
                if hello.format_version != 1
                    || hello.nonce != shared.nonce
                    || hello.scope != shared.scope
                    || hello.max_jobs != shared.max_jobs
                    || hello.max_inflight_bytes != shared.max_bytes
                {
                    return Err(SourceError::Protocol);
                }
                state.hello = true;
                return Ok(());
            }
            if !state.hello {
                return Err(SourceError::Protocol);
            }
            if frame.kind == FATAL || frame.kind == SHUTDOWN_ACK {
                if frame.id != 0 {
                    return Err(SourceError::Protocol);
                }
                return Err(SourceError::Closed);
            }
            let pending = state
                .pending
                .get_mut(&frame.id)
                .ok_or(SourceError::Protocol)?;
            if pending.result.is_some() {
                return Err(SourceError::Protocol);
            }
            match frame.kind {
                DATA => {
                    let Expected::Object(reference) = &pending.expected else {
                        return Err(SourceError::Protocol);
                    };
                    let length = pending
                        .buffer
                        .len()
                        .checked_add(frame.body.len())
                        .ok_or(SourceError::Protocol)?;
                    if frame.body.is_empty() || length as u64 > reference.size_bytes {
                        return Err(SourceError::Protocol);
                    }
                    pending.buffer.extend(frame.body);
                }
                OBJECT_DONE => {
                    let Expected::Object(reference) = &pending.expected else {
                        return Err(SourceError::Protocol);
                    };
                    if !frame.body.is_empty() {
                        return Err(SourceError::Protocol);
                    }
                    reference
                        .verify(&pending.buffer)
                        .map_err(|_| SourceError::Protocol)?;
                    pending.result =
                        Some(Ok(Response::Object(std::mem::take(&mut pending.buffer))));
                }
                ROOT_DONE => {
                    let Expected::Root(expected) = &pending.expected else {
                        return Err(SourceError::Protocol);
                    };
                    if !pending.buffer.is_empty() {
                        return Err(SourceError::Protocol);
                    }
                    let receipt: ReplicatedRootReceipt =
                        serde_json::from_slice(&frame.body).map_err(|_| SourceError::Protocol)?;
                    receipt.validate(expected)?;
                    pending.result = Some(Ok(Response::Root(receipt)));
                }
                ERROR => {
                    let error: WireError =
                        serde_json::from_slice(&frame.body).map_err(|_| SourceError::Protocol)?;
                    let error = match error.code.as_str() {
                        "busy" => SourceError::Busy,
                        "cancelled" => SourceError::Cancelled,
                        "timeout" => SourceError::TimedOut,
                        "failed" => SourceError::RemoteFailed,
                        _ => return Err(SourceError::Protocol),
                    };
                    pending.buffer.clear();
                    pending.result = Some(Err(error));
                }
                _ => return Err(SourceError::Protocol),
            }
            Ok(())
        })();
        match result {
            Ok(()) => shared.changed.notify_all(),
            Err(error) => {
                shared.fail(error);
                return;
            }
        }
    }
}
fn stderr_loop(shared: Arc<Shared>, mut reader: impl Read) {
    let mut buffer = [0; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(length) => {
                let total = shared
                    .stderr_bytes
                    .fetch_add(length as u64, Ordering::AcqRel)
                    .saturating_add(length as u64);
                if total > STDERR_LIMIT {
                    shared.fail(SourceError::Protocol);
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                shared.fail(SourceError::Io);
                return;
            }
        }
    }
}
