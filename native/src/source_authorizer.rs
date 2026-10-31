//! Host-only source authorization transport; managed integration is separate.
//! A protected context is connection DATA. Only an actual authenticated exchange
//! with independently held issuer-peer/custody input can initialize bounded
//! freshness. MAC proves endpoint-key authentication, never EngineOwnership.
//! No saved response constructs owner/source liveness or endpoint settlement.
use anyhow::{anyhow, ensure, Result};
use hmac::{Hmac, Mac};
use rudder_engine::{
    fleet_lease::{HostClock, SuspendAwareClock},
    journal::HostSourceAuthorization,
    protocol::canonical_json,
    vfs::partial_rpc::PartialBinding,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub(crate) const PROTOCOL: &str = "rudder_host_source_authorizer_v1";
// Matches the separate Engine Journal patch's HOST_SOURCE_PURPOSE. The later
// Root adapter calls authorize_host_source, required op source:host:authorize,
// and host_source_scope(binding); this transport never mints that capability.
pub(crate) const PURPOSE: &str = "remote_source_only_v1";
const CONTEXT: &str = "source-context.json";
const SOCKET: &str = "source.sock";
const MAX_FRAME: usize = 16 * 1024;
const MAX_REPLY: usize = 1024;
const FRESH_NS: u128 = 3_000_000_000;
const POLL: Duration = Duration::from_millis(10);
const REQUEST_DOMAIN: &[u8] = b"rudder_host_source_authorizer_request_v1\0";
const REPLY_DOMAIN: &[u8] = b"rudder_host_source_authorizer_reply_v1\0";
const NS_PER_MS: u128 = 1_000_000;

fn fault() -> anyhow::Error {
    anyhow!("host source authorization unavailable")
}
type Clock = Arc<dyn Fn() -> Result<u128> + Send + Sync>;
fn wall_nanoseconds() -> Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .map_err(|_| fault())
}
fn host_clock() -> Result<Clock> {
    let clock = HostClock::new().map_err(|_| fault())?;
    Ok(Arc::new(move || {
        clock
            .read()
            .map(|value| value.nanoseconds())
            .map_err(|_| fault())
    }))
}

/// Trusted host interface, never constructed by deserializing owner/token data.
/// Production must close over the actual EngineOwnership/Journal issuer and
/// independently check current owner, run/attempt, expiry and revocation on
/// every query. Return the actual non-Serde Journal snapshot; it is expiry DATA,
/// not authority. The transport supplies no issuer or remote-launch activation.
/// A blocked callback retains its fixed worker, stream and resource owner even
/// after its client deadline or close request. Errors are never printed/copied.
pub(crate) trait HostSourceAuthority: Send + Sync + 'static {
    fn authorize(&self, binding: &PartialBinding) -> Result<HostSourceAuthorization>;
}

/// REQUIRED independently held issuer/process custody, not the context secret,
/// saved PID, peer UID, echoed owner fields or Journal snapshot. There is no
/// production implementation/default in this patch. Later integration must bind
/// this to actual owned process/FD custody plus EngineSourceLease as appropriate.
/// Checks must be bounded and run off-App. No serialized restore is permitted.
pub(crate) trait HostSourceIssuerPeer: Send + Sync + 'static {
    fn check_current(&self) -> Result<()>;
    fn check_peer(&self, stream: &UnixStream) -> Result<()>;
}

/// Request-side custody of the actual owned helper supervisor. Production
/// adapters require a handoff-owned process witness, never a saved PID or UID.
pub(crate) trait HostSourceRequestPeer: Send + Sync + 'static {
    fn check_peer(&self, stream: &UnixStream) -> Result<()>;
}

#[derive(Clone)]
pub(crate) struct Limits {
    pub workers: usize,
    pub max_admitted: usize,
    pub requests_per_second: usize,
    pub request_timeout: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            workers: 2,
            max_admitted: 4,
            requests_per_second: 16,
            request_timeout: Duration::from_secs(2),
        }
    }
}
impl Limits {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=4).contains(&self.workers)
                && (self.workers..=8).contains(&self.max_admitted)
                && (1..=32).contains(&self.requests_per_second)
                && !self.request_timeout.is_zero()
                && self.request_timeout <= Duration::from_secs(2),
            "invalid host source limits"
        );
        Ok(())
    }
}

// Secret connection DATA, not Debug/Clone/public permission. Persisted only in
// a fresh host-only0700 directory, never in guest projection/env/CAS or logs.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextV1 {
    format_version: u32,
    protocol: String,
    purpose: String,
    server_instance: String,
    binding_digest: String,
    credential: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBody {
    format_version: u32,
    protocol: String,
    purpose: String,
    server_instance: String,
    request_id: String,
    serial: u64,
    binding: PartialBinding,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    body: RequestBody,
    mac: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyBody {
    format_version: u32,
    protocol: String,
    purpose: String,
    server_instance: String,
    request_id: String,
    serial: u64,
    binding_digest: String,
    request_mac: [u8; 32],
    authorized: bool,
    /// Fresh post-query remaining lifetime, conservatively anchored by the
    /// client to its ORIGINAL sent boottime, never response arrival.
    valid_for_ns: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    body: ReplyBody,
    mac: [u8; 32],
}
#[derive(Serialize)]
struct Authenticated<'a, T> {
    binding: &'a PartialBinding,
    message: &'a T,
}
fn authentication<T: Serialize>(
    credential: &str,
    domain: &[u8],
    binding: &PartialBinding,
    message: &T,
) -> Result<Hmac<Sha256>> {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(credential.as_bytes()).map_err(|_| fault())?;
    mac.update(domain);
    mac.update(&canonical_json(&Authenticated { binding, message }).map_err(|_| fault())?);
    Ok(mac)
}
fn sign<T: Serialize>(
    credential: &str,
    domain: &[u8],
    binding: &PartialBinding,
    message: &T,
) -> Result<[u8; 32]> {
    Ok(authentication(credential, domain, binding, message)?
        .finalize()
        .into_bytes()
        .into())
}
fn verify<T: Serialize>(
    credential: &str,
    domain: &[u8],
    binding: &PartialBinding,
    message: &T,
    tag: &[u8; 32],
) -> Result<()> {
    authentication(credential, domain, binding, message)?
        .verify_slice(tag)
        .map_err(|_| fault())
}
fn uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
}
fn private_file(metadata: &fs::Metadata, uid: u32, mode: u32) -> bool {
    metadata.is_file()
        && metadata.uid() == uid
        && metadata.nlink() == 1
        && metadata.mode() & 0o7777 == mode
}
fn same(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}
fn open_directory(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| fault())?;
    let metadata = file.metadata().map_err(|_| fault())?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o7777 == 0o700,
        "host source context is not private"
    );
    Ok(file)
}
fn leaf(file: &File, name: &str, flags: i32) -> Result<File> {
    let name = std::ffi::CString::new(name).map_err(|_| fault())?;
    let fd = unsafe {
        libc::openat(
            file.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(fault());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn read_context(directory: &File, path: &Path) -> Result<ContextV1> {
    let mut file = leaf(directory, CONTEXT, libc::O_RDONLY)?;
    let start = file.metadata().map_err(|_| fault())?;
    ensure!(
        private_file(&start, unsafe { libc::geteuid() }, 0o600)
            && start.len() > 0
            && start.len() <= MAX_FRAME as u64,
        "unsafe host source context"
    );
    let mut bytes = Vec::with_capacity(start.len() as usize);
    (&mut file)
        .take(MAX_FRAME as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| fault())?;
    let end = file.metadata().map_err(|_| fault())?;
    let named = fs::symlink_metadata(path.join(CONTEXT)).map_err(|_| fault())?;
    ensure!(
        bytes.len() == start.len() as usize
            && bytes.len() <= MAX_FRAME
            && same(&start, &end)
            && same(&start, &named)
            && private_file(&end, unsafe { libc::geteuid() }, 0o600)
            && private_file(&named, unsafe { libc::geteuid() }, 0o600)
            && end.len() == start.len()
            && named.len() == start.len(),
        "host source context changed"
    );
    serde_json::from_slice(&bytes).map_err(|_| fault())
}
fn peer_uid(stream: &UnixStream) -> Result<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut value: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&value) as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut value as *mut libc::ucred).cast(),
                    &mut size,
                )
            } == 0
                && size as usize == std::mem::size_of_val(&value),
            "host source peer unavailable"
        );
        Ok(value.uid)
    }
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        ensure!(
            unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0,
            "host source peer unavailable"
        );
        Ok(uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = stream;
        Err(fault())
    }
}
fn configure(stream: &UnixStream) -> Result<()> {
    stream.set_nonblocking(false).map_err(|_| fault())?;
    stream.set_read_timeout(Some(POLL)).map_err(|_| fault())?;
    stream.set_write_timeout(Some(POLL)).map_err(|_| fault())
}
fn connect_socket(
    path: &Path,
    deadline: Instant,
    check: impl Fn() -> Result<()>,
) -> Result<UnixStream> {
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    ensure!(
        !bytes.is_empty() && bytes.len() < address.sun_path.len() && !bytes.contains(&0),
        "invalid host source socket path"
    );
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(bytes) {
        *target = *source as libc::c_char;
    }
    let size = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = size as u8;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    ensure!(fd >= 0, "host source socket unavailable");
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    ensure!(
        flags >= 0
            && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0
            && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
        "host source socket flags unavailable"
    );
    loop {
        check()?;
        ensure!(Instant::now() < deadline, "host source connection deadline");
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                size as libc::socklen_t,
            )
        };
        if result == 0 {
            break;
        }
        let errno = io::Error::last_os_error().raw_os_error();
        if errno == Some(libc::EINTR) {
            continue;
        }
        ensure!(
            matches!(
                errno,
                Some(libc::EINPROGRESS | libc::EAGAIN | libc::EALREADY)
            ),
            "host source connection rejected"
        );
        loop {
            check()?;
            ensure!(Instant::now() < deadline, "host source connection deadline");
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            if unsafe { libc::poll(&mut poll, 1, 10) } > 0 {
                let mut error = 0i32;
                let mut length = std::mem::size_of_val(&error) as libc::socklen_t;
                ensure!(
                    unsafe {
                        libc::getsockopt(
                            fd.as_raw_fd(),
                            libc::SOL_SOCKET,
                            libc::SO_ERROR,
                            (&mut error as *mut i32).cast(),
                            &mut length,
                        )
                    } == 0
                        && error == 0,
                    "host source connection rejected"
                );
                // Linux AF_UNIX EAGAIN means the listen queue was full and no
                // connect started. Retry nonblocking, never block before a cap.
                if errno == Some(libc::EAGAIN) {
                    thread::sleep(POLL);
                    break;
                }
                return Ok(UnixStream::from(fd));
            }
        }
    }
    Ok(UnixStream::from(fd))
}
fn io_until(
    stream: &mut UnixStream,
    bytes: &mut [u8],
    write: bool,
    deadline: Instant,
    check: impl Fn() -> Result<()>,
) -> Result<()> {
    let mut position = 0;
    while position < bytes.len() {
        check()?;
        ensure!(Instant::now() < deadline, "host source frame deadline");
        let result = if write {
            stream.write(&bytes[position..])
        } else {
            stream.read(&mut bytes[position..])
        };
        match result {
            Ok(0) => return Err(fault()),
            Ok(count) => position += count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(fault()),
        }
    }
    check()?;
    ensure!(Instant::now() < deadline, "host source frame deadline");
    Ok(())
}
fn receive<T: DeserializeOwned>(
    stream: &mut UnixStream,
    max: usize,
    deadline: Instant,
    check: impl Fn() -> Result<()>,
) -> Result<T> {
    let mut size = [0u8; 4];
    io_until(stream, &mut size, false, deadline, &check)?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(size > 0 && size <= max, "host source frame exceeds bound");
    let mut bytes = vec![0; size];
    io_until(stream, &mut bytes, false, deadline, check)?;
    serde_json::from_slice(&bytes).map_err(|_| fault())
}
fn send<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
    max: usize,
    deadline: Instant,
    check: impl Fn() -> Result<()>,
) -> Result<()> {
    let bytes = canonical_json(value).map_err(|_| fault())?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= max,
        "host source frame exceeds bound"
    );
    let mut frame = Vec::with_capacity(bytes.len() + 4);
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&bytes);
    io_until(stream, &mut frame, true, deadline, check)
}

struct Worker {
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> Result<Self> {
        let thread = thread::Builder::new()
            .name(name.into())
            .spawn(work)
            .map_err(|_| fault())?;
        Ok(Self {
            thread: Some(thread),
        })
    }
}
/// One fixed join owner per endpoint, never per request. Real worker joins may
/// block through arbitrary callback/TLS destruction here without blocking the
/// caller's close deadline or releasing actual handle/State custody.
struct JoinCoordinator {
    _thread: JoinHandle<()>,
    done: mpsc::Receiver<bool>,
    workers_joined: bool,
    failed: bool,
}
impl JoinCoordinator {
    fn start(workers: &mut Vec<Worker>, state: Arc<State>) -> Result<Self> {
        let owned = Arc::new(Mutex::new(Some(std::mem::take(workers))));
        let joining = Arc::clone(&owned);
        let (send, done) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("rudder-host-source-join".into())
            .spawn(move || {
                let workers = joining.lock().unwrap().take().unwrap();
                let mut success = true;
                for mut worker in workers {
                    if worker.thread.take().unwrap().join().is_err() {
                        state.fence();
                        success = false;
                    }
                }
                // Source/authority resources stay held through every REAL
                // worker join, including user thread-local destructors.
                drop(state);
                let _ = send.send(success);
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(_) => {
                // A failed coordinator spawn must not lose worker handles.
                *workers = owned
                    .lock()
                    .map_err(|_| fault())?
                    .take()
                    .ok_or_else(fault)?;
                return Err(fault());
            }
        };
        Ok(Self {
            _thread: thread,
            done,
            workers_joined: false,
            failed: false,
        })
    }
    fn wait(&mut self, deadline: Instant) -> Result<()> {
        ensure!(!self.failed, "host source join failed");
        if self.workers_joined {
            return Ok(());
        }
        match self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(true) => self.workers_joined = true,
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(fault()),
            _ => {
                self.failed = true;
                return Err(fault());
            }
        }
        // Do NOT join this coordinator based on a completion message or
        // is_finished: that would recreate the TLS deadline bug. The opaque
        // settlement retains its handle; only endpoint workers/socket work
        // have been proven drained, not this fixed observer's final teardown.
        Ok(())
    }
}

struct Directory {
    path: PathBuf,
    fd: File,
    identity: fs::Metadata,
}
impl Directory {
    fn verify(&self) -> Result<()> {
        let held = self.fd.metadata().map_err(|_| fault())?;
        let named = fs::symlink_metadata(&self.path).map_err(|_| fault())?;
        ensure!(
            same(&self.identity, &held)
                && same(&self.identity, &named)
                && named.is_dir()
                && named.uid() == unsafe { libc::geteuid() }
                && named.mode() & 0o7777 == 0o700,
            "host source directory changed"
        );
        Ok(())
    }
    // There is no POSIX atomic compare-and-unlink. Even a just-verified path
    // can be replaced before removal. This module NEVER unlinks endpoint paths
    // or removes directories, including on settlement/startup failure/Drop.
    // Explicit owner-serialized recovery is outside this transport API.
}

// Four seconds of bounded credit permits finalization's fresh-check burst
// without adding sleeps. Sustained ingress remains limited to the configured
// rate; fixed workers/admissions independently bound actual concurrent work.
struct RequestRate {
    last: Instant,
    credits: u128,
    capacity: u128,
    per_second: u128,
}
impl RequestRate {
    const UNIT: u128 = 1_000_000_000;
    fn new(now: Instant, per_second: usize) -> Self {
        let capacity = per_second as u128 * 4 * Self::UNIT;
        Self {
            last: now,
            credits: capacity,
            capacity,
            per_second: per_second as u128,
        }
    }
    fn admit(&mut self, now: Instant) -> Result<()> {
        let elapsed = now.checked_duration_since(self.last).ok_or_else(fault)?;
        self.last = now;
        self.credits = self.capacity.min(
            self.credits
                .saturating_add(elapsed.as_nanos().saturating_mul(self.per_second)),
        );
        ensure!(self.credits >= Self::UNIT, "host source rate bound");
        self.credits -= Self::UNIT;
        Ok(())
    }
}

struct State {
    binding: PartialBinding,
    binding_digest: String,
    context: ContextV1,
    authority: Arc<dyn HostSourceAuthority>,
    request_peer: Option<Arc<dyn HostSourceRequestPeer>>,
    limits: Limits,
    directory: Directory,
    stopped: AtomicBool,
    fenced: AtomicBool,
    admitted: AtomicUsize,
    calls: AtomicUsize,
    serial: AtomicU64,
    sockets: Mutex<BTreeMap<u64, UnixStream>>,
    rate: Mutex<RequestRate>,
    wall_clock: Clock,
    last_wall: Mutex<Option<u128>>,
}
impl State {
    fn check(&self) -> Result<()> {
        ensure!(
            !self.stopped.load(Ordering::Acquire) && !self.fenced.load(Ordering::Acquire),
            "host source endpoint retired"
        );
        Ok(())
    }
    fn close(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Ok(sockets) = self.sockets.lock() {
            for stream in sockets.values() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
    fn fence_except(&self, exception: Option<u64>) {
        self.fenced.store(true, Ordering::Release);
        self.stopped.store(true, Ordering::Release);
        if let Ok(sockets) = self.sockets.lock() {
            for (id, stream) in sockets.iter() {
                if Some(*id) != exception {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }
    }
    fn fence(&self) {
        self.fence_except(None);
    }
    fn wall(&self) -> Result<u128> {
        let mut last = self.last_wall.lock().map_err(|_| fault())?;
        let now = (self.wall_clock)()?;
        ensure!(
            last.is_none_or(|last| now >= last),
            "host source clock regressed"
        );
        *last = Some(now);
        Ok(now)
    }
    fn remaining(&self, grant: &HostSourceAuthorization, before: u128) -> Result<u64> {
        // Sample AFTER actual query completion. Multiplication/subtraction is
        // checked at nanosecond precision; never round remaining TTL upward.
        let now = self.wall()?;
        let validated = u128::try_from(grant.validated_at_ms)
            .ok()
            .and_then(|value| value.checked_mul(NS_PER_MS))
            .ok_or_else(fault)?;
        ensure!(
            validated <= now
                && grant.validated_at_ms
                    >= i64::try_from(before / NS_PER_MS).map_err(|_| fault())?
                && now.saturating_sub(validated) < FRESH_NS
                && grant.identity.project_id == self.binding.attempt.project_id
                && grant.identity.run_id == self.binding.attempt.run_id
                && grant.identity.run_epoch == self.binding.attempt.run_epoch
                && grant.identity.owner_epoch == self.binding.attempt.owner.epoch,
            "host source snapshot unavailable"
        );
        let remaining = match grant.expires_at_ms {
            Some(expiry) => {
                let expiry = u128::try_from(expiry)
                    .ok()
                    .and_then(|value| value.checked_mul(NS_PER_MS))
                    .ok_or_else(fault)?;
                expiry
                    .checked_sub(now)
                    .filter(|value| *value > 0)
                    .ok_or_else(fault)?
            }
            None => FRESH_NS,
        };
        u64::try_from(remaining.min(FRESH_NS)).map_err(|_| fault())
    }
    fn admit(self: &Arc<Self>, stream: UnixStream) -> Result<Connection> {
        self.check()?;
        let mut rate = self.rate.lock().map_err(|_| fault())?;
        rate.admit(Instant::now())?;
        drop(rate);
        let mut count = self.admitted.load(Ordering::Acquire);
        loop {
            ensure!(
                count < self.limits.max_admitted,
                "host source admission bound"
            );
            match self.admitted.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(next) => count = next,
            }
        }
        let id = self.serial.fetch_add(1, Ordering::AcqRel);
        let lease = Lease {
            state: Arc::clone(self),
            id,
        };
        let clone = match stream.try_clone() {
            Ok(value) => value,
            Err(_) => {
                drop(stream);
                drop(lease);
                return Err(fault());
            }
        };
        if self
            .sockets
            .lock()
            .map(|mut sockets| sockets.insert(id, clone))
            .is_err()
        {
            drop(stream);
            drop(lease);
            return Err(fault());
        }
        let connection = Connection {
            stream,
            _lease: lease,
        };
        if self.check().is_err() {
            let _ = connection.stream.shutdown(std::net::Shutdown::Both);
            return Err(fault());
        }
        Ok(connection)
    }
}
struct Lease {
    state: Arc<State>,
    id: u64,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut sockets) = self.state.sockets.lock() {
            sockets.remove(&self.id);
        }
        self.state.admitted.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Connection {
    stream: UnixStream,
    _lease: Lease,
}
struct Call(Arc<State>);
impl Drop for Call {
    fn drop(&mut self) {
        self.0.calls.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Must outlive finalization/receipt and actual helper exit. Guest ingress close
/// must not call close/fence here. No Clone, Serialize or saved reopen path.
pub(crate) struct SourceAuthorizerServer {
    state: Arc<State>,
    workers: Vec<Worker>,
    joiner: Option<JoinCoordinator>,
    settled: bool,
    request_only_drop: bool,
}
impl SourceAuthorizerServer {
    pub(crate) fn bind(
        path: &Path,
        binding: PartialBinding,
        authority: Arc<dyn HostSourceAuthority>,
        limits: Limits,
    ) -> Result<Self> {
        Self::with_wall_clock(path, binding, authority, limits, Arc::new(wall_nanoseconds))
    }
    pub(crate) fn bind_with_request_peer(
        path: &Path,
        binding: PartialBinding,
        authority: Arc<dyn HostSourceAuthority>,
        limits: Limits,
        peer: Arc<dyn HostSourceRequestPeer>,
    ) -> Result<Self> {
        Self::bind_inner(path, binding, authority, limits, Arc::new(wall_nanoseconds), Some(peer))
    }
    /// The caller creates this ownership slot before construction. Every
    /// successfully spawned worker stays in it even when later startup fails.
    pub(crate) fn bind_with_request_peer_retaining(
        path: &Path, binding: PartialBinding, authority: Arc<dyn HostSourceAuthority>,
        limits: Limits, peer: Arc<dyn HostSourceRequestPeer>,
        owner: &mut Option<Self>,
    ) -> Result<()> {
        Self::bind_inner_retaining(path, binding, authority, limits,
            Arc::new(wall_nanoseconds), Some(peer), owner)
    }
    fn with_wall_clock(
        path: &Path,
        binding: PartialBinding,
        authority: Arc<dyn HostSourceAuthority>,
        limits: Limits,
        wall_clock: Clock,
    ) -> Result<Self> {
        Self::bind_inner(path, binding, authority, limits, wall_clock, None)
    }
    fn bind_inner(
        path: &Path,
        binding: PartialBinding,
        authority: Arc<dyn HostSourceAuthority>,
        limits: Limits,
        wall_clock: Clock,
        request_peer: Option<Arc<dyn HostSourceRequestPeer>>,
    ) -> Result<Self> {
        let mut owner = None;
        Self::bind_inner_retaining(path, binding, authority, limits, wall_clock,
            request_peer, &mut owner)?;
        let mut server = owner.take().unwrap();
        server.request_only_drop = false;
        Ok(server)
    }
    fn bind_inner_retaining(
        path: &Path, binding: PartialBinding, authority: Arc<dyn HostSourceAuthority>,
        limits: Limits, wall_clock: Clock,
        request_peer: Option<Arc<dyn HostSourceRequestPeer>>, owner: &mut Option<Self>,
    ) -> Result<()> {
        ensure!(owner.is_none(), "host source construction slot already owns resources");
        limits.validate()?;
        binding.validate().map_err(|_| fault())?;
        let binding_digest = binding.digest().map_err(|_| fault())?;
        ensure!(
            path.is_absolute()
                && path
                    .components()
                    .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
            "invalid host source path"
        );
        let parent = path
            .parent()
            .ok_or_else(fault)?
            .canonicalize()
            .map_err(|_| fault())?;
        let path = parent.join(path.file_name().ok_or_else(fault)?);
        ensure!(
            path.join(SOCKET).as_os_str().as_encoded_bytes().len() < 104,
            "host source socket path too long"
        );
        let context = ContextV1 {
            format_version: 1,
            protocol: PROTOCOL.into(),
            purpose: PURPOSE.into(),
            server_instance: Uuid::new_v4().to_string(),
            binding_digest: binding_digest.clone(),
            credential: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        };
        let context_bytes = canonical_json(&context).map_err(|_| fault())?;
        ensure!(
            context_bytes.len() <= MAX_FRAME,
            "host source context exceeds bound"
        );
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| fault())?;
        let fd = open_directory(&path)?;
        let identity = fd.metadata().map_err(|_| fault())?;
        let listener = UnixListener::bind(path.join(SOCKET)).map_err(|_| fault())?;
        ensure!(
            unsafe { libc::listen(listener.as_raw_fd(), limits.max_admitted as i32) } == 0,
            "host source listen bound unavailable"
        );
        fs::set_permissions(path.join(SOCKET), fs::Permissions::from_mode(0o600))
            .map_err(|_| fault())?;
        listener.set_nonblocking(true).map_err(|_| fault())?;
        let mut file = leaf(&fd, CONTEXT, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        file.write_all(&context_bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| fault())?;
        let directory = Directory { path, fd, identity };
        directory.verify()?;
        directory.fd.sync_all().map_err(|_| fault())?;
        let state = Arc::new(State {
            binding,
            binding_digest,
            context,
            authority,
            request_peer,
            limits: limits.clone(),
            directory,
            stopped: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
            admitted: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            serial: AtomicU64::new(1),
            sockets: Mutex::new(BTreeMap::new()),
            rate: Mutex::new(RequestRate::new(Instant::now(), limits.requests_per_second)),
            wall_clock,
            last_wall: Mutex::new(None),
        });
        *owner = Some(Self {
            state: Arc::clone(&state), workers: Vec::with_capacity(limits.workers + 1),
            joiner: None, settled: false, request_only_drop: true,
        });
        let server = owner.as_mut().unwrap();
        let (send, receive) = mpsc::sync_channel::<Connection>(limits.max_admitted);
        let receive = Arc::new(Mutex::new(receive));
        for _ in 0..limits.workers {
            let state = Arc::clone(&state);
            let receive = Arc::clone(&receive);
            server
                .workers
                .push(Worker::spawn("rudder-host-source-call", move || {
                    let mut exit = ExitFence::new(Arc::clone(&state));
                    while state.check().is_ok() {
                        let result = receive.lock().unwrap().recv_timeout(POLL);
                        match result {
                            Ok(mut connection) => {
                                let _ = call(&state, &mut connection.stream, connection._lease.id);
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(_) => break,
                        }
                    }
                    while receive.lock().unwrap().try_recv().is_ok() {}
                    exit.completed = true;
                })?);
        }
        server
            .workers
            .push(Worker::spawn("rudder-host-source-accept", move || {
                let mut exit = ExitFence::new(Arc::clone(&state));
                while state.check().is_ok() {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Ok(connection) = state.admit(stream) {
                                let _ = send.try_send(connection);
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(POLL)
                        }
                        Err(_) => {
                            state.fence();
                            break;
                        }
                    }
                }
                drop(listener);
                exit.completed = true;
            })?);
        Ok(())
    }
    pub(crate) fn directory(&self) -> &Path {
        &self.state.directory.path
    }
    pub(crate) fn close(&self) {
        self.state.close();
    }
    pub(crate) fn await_closed(&mut self, timeout: Duration) -> Result<SourceAuthorizerSettled> {
        ensure!(!self.settled, "host source settlement already returned");
        self.close();
        let deadline = Instant::now().checked_add(timeout).ok_or_else(fault)?;
        if self.joiner.is_none() {
            self.joiner = Some(JoinCoordinator::start(
                &mut self.workers,
                Arc::clone(&self.state),
            )?);
        }
        self.joiner.as_mut().unwrap().wait(deadline)?;
        ensure!(
            self.state.admitted.load(Ordering::Acquire) == 0
                && self.state.calls.load(Ordering::Acquire) == 0
                && self.state.sockets.lock().map_err(|_| fault())?.is_empty(),
            "host source work still owned"
        );
        // Actual endpoint work is settled. Leave ALL pathnames untouched,
        // including replacements made after any prior identity check.
        self.settled = true;
        Ok(SourceAuthorizerSettled {
            _state: Arc::clone(&self.state),
            _joiner: self.joiner.take().unwrap(),
        })
    }
}
impl Drop for SourceAuthorizerServer {
    fn drop(&mut self) {
        // Destruction requests retirement only. Explicit off-App await_closed
        // owns socket shutdown/locks, actual worker joins and settlement proof.
        if self.request_only_drop { self.state.stopped.store(true, Ordering::Release); }
        else { self.close(); }
    }
}
/// Endpoint-only actual settlement, not pathname cleanup, source transport,
/// PID/FS/checkpoint or continued owner authority. No Deserialize, Clone or public constructor.
pub(crate) struct SourceAuthorizerSettled {
    _state: Arc<State>,
    _joiner: JoinCoordinator,
}

struct ExitFence {
    state: Arc<State>,
    completed: bool,
}
impl ExitFence {
    fn new(state: Arc<State>) -> Self {
        Self {
            state,
            completed: false,
        }
    }
}
impl Drop for ExitFence {
    fn drop(&mut self) {
        if !self.completed {
            self.state.fence();
        }
    }
}
fn reply(state: &State, request: &Request, authorized: bool, valid_for_ns: u64) -> Result<Reply> {
    let body = ReplyBody {
        format_version: 1,
        protocol: PROTOCOL.into(),
        purpose: PURPOSE.into(),
        server_instance: state.context.server_instance.clone(),
        request_id: request.body.request_id.clone(),
        serial: request.body.serial,
        binding_digest: state.binding_digest.clone(),
        request_mac: request.mac,
        authorized,
        valid_for_ns,
    };
    let mac = sign(
        &state.context.credential,
        REPLY_DOMAIN,
        &state.binding,
        &body,
    )?;
    Ok(Reply { body, mac })
}
fn call(state: &Arc<State>, stream: &mut UnixStream, connection_id: u64) -> Result<()> {
    configure(stream)?;
    ensure!(
        peer_uid(stream)? == unsafe { libc::geteuid() },
        "host source peer rejected"
    );
    // Refuse an unbound or foreign supervisor before reading the request or
    // invoking Journal authority. A failed peer is not source revocation.
    if let Some(peer) = &state.request_peer {
        let checked = catch_unwind(AssertUnwindSafe(|| peer.check_peer(stream)));
        ensure!(matches!(checked, Ok(Ok(()))), "host source supervisor rejected");
    }
    let deadline = Instant::now() + state.limits.request_timeout;
    let request: Request = receive(stream, MAX_FRAME, deadline, || state.check())?;
    let body = &request.body;
    ensure!(
        body.format_version == 1
            && body.protocol == PROTOCOL
            && body.purpose == PURPOSE
            && body.server_instance == state.context.server_instance
            && uuid(&body.request_id)
            && body.serial > 0
            && body.binding == state.binding,
        "host source request rejected"
    );
    body.binding.validate().map_err(|_| fault())?;
    verify(
        &state.context.credential,
        REQUEST_DOMAIN,
        &state.binding,
        body,
        &request.mac,
    )?;
    state.check()?;
    state.calls.fetch_add(1, Ordering::AcqRel);
    let _call = Call(Arc::clone(state));
    let authorization = catch_unwind(AssertUnwindSafe(|| {
        // Run this guard INSIDE the unwind boundary: a callback panic fences
        // every stream while unwinding, before catch_unwind returns and before
        // a surviving worker can issue any later positive reply.
        let mut unwind = ExitFence::new(Arc::clone(state));
        let result = (|| {
            let before = state.wall()?;
            let grant = state.authority.authorize(&body.binding)?;
            state.remaining(&grant, before)
        })();
        unwind.completed = true;
        result
    }));
    let valid_for_ns = match authorization {
        Ok(Ok(value)) => value,
        _ => {
            // Retire before emitting an authenticated negative reply. Only this
            // bounded denial connection remains open; every other stream closes.
            state.fence_except(Some(connection_id));
            let denied = reply(state, &request, false, 0)?;
            let result = send(stream, &denied, MAX_REPLY, deadline, || Ok(()));
            state.close();
            result?;
            return Err(fault());
        }
    };
    state.check()?;
    ensure!(
        Instant::now() < deadline,
        "host source callback returned late"
    );
    let authorized = reply(state, &request, true, valid_for_ns)?;
    if let Some(peer) = &state.request_peer {
        let checked = catch_unwind(AssertUnwindSafe(|| peer.check_peer(stream)));
        ensure!(matches!(checked, Ok(Ok(()))), "host source supervisor custody changed");
    }
    send(stream, &authorized, MAX_REPLY, deadline, || state.check())
}

struct Freshness {
    last: Option<u128>,
    until: Option<u128>,
    next: u64,
    accepted: u64,
}
struct ClientState {
    clock: Clock,
    issuer: Arc<dyn HostSourceIssuerPeer>,
    fenced: AtomicBool,
    fresh: Mutex<Freshness>,
}
impl ClientState {
    fn fence(&self) {
        self.fenced.store(true, Ordering::Release);
    }
    fn sample(&self, fresh: &mut Freshness, initial: bool) -> Result<u128> {
        ensure!(
            !self.fenced.load(Ordering::Acquire),
            "host source client fenced"
        );
        self.issuer.check_current()?;
        let now = (self.clock)()?;
        ensure!(
            fresh.last.is_none_or(|last| now >= last)
                && fresh.until.is_none_or(|until| now < until)
                && (initial || fresh.until.is_some()),
            "host source freshness unavailable"
        );
        fresh.last = Some(now);
        Ok(now)
    }
    fn begin(&self) -> Result<(u64, u128)> {
        let result = (|| {
            let mut fresh = self.fresh.lock().map_err(|_| fault())?;
            let now = self.sample(&mut fresh, true)?;
            fresh.next = fresh.next.checked_add(1).ok_or_else(fault)?;
            Ok((fresh.next, now))
        })();
        if result.is_err() {
            self.fence();
        }
        result
    }
    fn accept(&self, serial: u64, sent: u128, valid_for_ns: u64) -> Result<()> {
        let result = (|| {
            let mut fresh = self.fresh.lock().map_err(|_| fault())?;
            let now = self.sample(&mut fresh, true)?;
            ensure!(
                valid_for_ns > 0 && u128::from(valid_for_ns) <= FRESH_NS,
                "invalid host source authorization lifetime"
            );
            let until = sent
                .checked_add(u128::from(valid_for_ns))
                .ok_or_else(fault)?;
            ensure!(now < until, "late host source reply");
            if serial > fresh.accepted {
                fresh.accepted = serial;
                fresh.until = Some(until);
            }
            Ok(())
        })();
        if result.is_err() {
            self.fence();
        }
        result
    }
    fn check(&self) -> Result<()> {
        let result = (|| {
            let mut fresh = self.fresh.lock().map_err(|_| fault())?;
            self.sample(&mut fresh, false)?;
            Ok(())
        })();
        if result.is_err() {
            self.fence();
        }
        result
    }
    fn inflight(&self, sent: u128) -> Result<()> {
        let result = (|| {
            let mut fresh = self.fresh.lock().map_err(|_| fault())?;
            let now = self.sample(&mut fresh, true)?;
            ensure!(
                now < sent.checked_add(FRESH_NS).ok_or_else(fault)?,
                "host source call freshness expired"
            );
            Ok(())
        })();
        if result.is_err() {
            self.fence();
        }
        result
    }
}
/// A real host client, not a saved lease. Requires independent issuer custody,
/// then an actual MAC/nonce-bound exchange before bounded freshness exists.
/// Cached freshness is not instant Journal/run revocation. Actual retirement
/// must invalidate BOTH this client and its RemoteSession in the future owner.
pub(crate) struct SourceAuthorizerClient {
    directory: PathBuf,
    held_directory: File,
    directory_identity: fs::Metadata,
    context: ContextV1,
    binding: PartialBinding,
    state: ClientState,
    timeout: Duration,
}
impl SourceAuthorizerClient {
    pub(crate) fn connect(
        directory: &Path,
        binding: PartialBinding,
        issuer: Arc<dyn HostSourceIssuerPeer>,
    ) -> Result<Self> {
        Self::with_clock(directory, binding, issuer, host_clock()?)
    }
    fn with_clock(
        directory: &Path,
        binding: PartialBinding,
        issuer: Arc<dyn HostSourceIssuerPeer>,
        clock: Clock,
    ) -> Result<Self> {
        issuer.check_current()?;
        binding.validate().map_err(|_| fault())?;
        let directory = directory.canonicalize().map_err(|_| fault())?;
        let held_directory = open_directory(&directory)?;
        let directory_identity = held_directory.metadata().map_err(|_| fault())?;
        let context = read_context(&held_directory, &directory)?;
        ensure!(
            context.format_version == 1
                && context.protocol == PROTOCOL
                && context.purpose == PURPOSE
                && uuid(&context.server_instance)
                && context.binding_digest == binding.digest().map_err(|_| fault())?
                && context.credential.len() == 64
                && context
                    .credential
                    .bytes()
                    .all(|value| value.is_ascii_hexdigit() && !value.is_ascii_uppercase()),
            "host source context binding rejected"
        );
        let client = Self {
            directory,
            held_directory,
            directory_identity,
            context,
            binding,
            state: ClientState {
                clock,
                issuer,
                fenced: AtomicBool::new(false),
                fresh: Mutex::new(Freshness {
                    last: None,
                    until: None,
                    next: 0,
                    accepted: 0,
                }),
            },
            timeout: Duration::from_secs(2),
        };
        client.authorize(&client.binding)?;
        Ok(client)
    }
    pub(crate) fn authorize(&self, binding: &PartialBinding) -> Result<()> {
        let result = (|| {
            ensure!(
                *binding == self.binding,
                "host source client binding changed"
            );
            let (serial, sent) = self.state.begin()?;
            let deadline = Instant::now() + self.timeout;
            let held = self.held_directory.metadata().map_err(|_| fault())?;
            let named = fs::symlink_metadata(&self.directory).map_err(|_| fault())?;
            ensure!(
                same(&self.directory_identity, &held)
                    && same(&self.directory_identity, &named)
                    && named.is_dir()
                    && named.uid() == unsafe { libc::geteuid() }
                    && named.mode() & 0o7777 == 0o700,
                "host source directory changed"
            );
            let socket = fs::symlink_metadata(self.directory.join(SOCKET)).map_err(|_| fault())?;
            ensure!(
                socket.file_type().is_socket()
                    && socket.uid() == unsafe { libc::geteuid() }
                    && socket.mode() & 0o7777 == 0o600,
                "host source socket changed"
            );
            let mut stream = connect_socket(&self.directory.join(SOCKET), deadline, || {
                self.state.inflight(sent)
            })?;
            configure(&stream)?;
            ensure!(
                peer_uid(&stream)? == unsafe { libc::geteuid() },
                "host source peer rejected"
            );
            self.state.issuer.check_peer(&stream)?;
            self.state.issuer.check_current()?;
            let body = RequestBody {
                format_version: 1,
                protocol: PROTOCOL.into(),
                purpose: PURPOSE.into(),
                server_instance: self.context.server_instance.clone(),
                request_id: Uuid::new_v4().to_string(),
                serial,
                binding: binding.clone(),
            };
            let mac = sign(&self.context.credential, REQUEST_DOMAIN, binding, &body)?;
            let request = Request { body, mac };
            send(&mut stream, &request, MAX_FRAME, deadline, || {
                self.state.inflight(sent)
            })?;
            let response: Reply = receive(&mut stream, MAX_REPLY, deadline, || {
                self.state.inflight(sent)
            })?;
            verify(
                &self.context.credential,
                REPLY_DOMAIN,
                binding,
                &response.body,
                &response.mac,
            )?;
            let reply = response.body;
            ensure!(
                reply.format_version == 1
                    && reply.protocol == PROTOCOL
                    && reply.purpose == PURPOSE
                    && reply.server_instance == self.context.server_instance
                    && reply.request_id == request.body.request_id
                    && reply.serial == serial
                    && reply.binding_digest == self.context.binding_digest
                    && reply.request_mac == request.mac
                    && reply.authorized,
                "host source reply rejected"
            );
            self.state.accept(serial, sent, reply.valid_for_ns)
        })();
        if result.is_err() {
            self.state.fence();
        }
        result.map_err(|_| fault())
    }
    pub(crate) fn check_current(&self) -> Result<()> {
        self.state.check().map_err(|_| fault())
    }
    pub(crate) fn invalidate(&self) {
        self.state.fence();
    }
}

#[cfg(test)]
#[path = "source_authorizer_tests.rs"]
mod tests;
