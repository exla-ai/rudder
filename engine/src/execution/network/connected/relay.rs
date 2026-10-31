use super::{
    resolver::ResolverPool,
    socket::{self, HostNetworkAnchor, ListenerAdmission, ListenerWitnesses},
    ConnectAuthority, ConnectBinding, ConnectError as Error, ConnectErrorKind as Kind,
    ConnectResult as Result, ResolverProgram, MAX_LIFETIME_CONNECTIONS,
};
use crate::execution::network::{HostIpv4DenySet, ValidatedNetworkProfile};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    os::fd::{AsRawFd, RawFd},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Strict first CONNECT request only. Headers are discarded; none reach the
/// origin. Arbitrary methods, auth, bodies and already-buffered early data fail.
#[derive(Debug, PartialEq, Eq)]
pub struct ConnectRequest {
    pub host: String,
    pub port: u16,
}
impl ConnectRequest {
    pub fn parse(bytes: &[u8], maximum: usize) -> Result<Self> {
        if bytes.is_empty()
            || bytes.len() > maximum.min(16 * 1024)
            || !bytes.is_ascii()
            || !bytes.ends_with(b"\r\n\r\n")
        {
            return Err(Error::new(Kind::InvalidRequest));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| Error::new(Kind::InvalidRequest))?;
        let mut lines = text.strip_suffix("\r\n\r\n").unwrap().split("\r\n");
        let first = lines
            .next()
            .ok_or_else(|| Error::new(Kind::InvalidRequest))?;
        let words: Vec<_> = first.split(' ').collect();
        if words.len() != 3 || words[0] != "CONNECT" || words[2] != "HTTP/1.1" {
            return Err(Error::new(Kind::InvalidRequest));
        }
        let Some(host) = words[1].strip_suffix(":443") else {
            return Err(Error::new(Kind::InvalidRequest));
        };
        if !super::super::canonical_public_hostname(host) {
            return Err(Error::new(Kind::InvalidRequest));
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut count = 0;
        for line in lines {
            if line.is_empty() {
                return Err(Error::new(Kind::InvalidRequest));
            }
            count += 1;
            if count > 16
                || line.len() > 1024
                || line.starts_with([' ', '\t'])
                || line.bytes().any(|b| b < 32 || b == 127)
            {
                return Err(Error::new(Kind::InvalidRequest));
            }
            let Some((key, value)) = line.split_once(':') else {
                return Err(Error::new(Kind::InvalidRequest));
            };
            let key = key.to_ascii_lowercase();
            if !seen.insert(key.clone()) {
                return Err(Error::new(Kind::InvalidRequest));
            }
            let value = value.trim_matches(' ');
            match key.as_str() {
                "host" if value == words[1] => {}
                "proxy-connection"
                    if value.eq_ignore_ascii_case("keep-alive")
                        || value.eq_ignore_ascii_case("close") => {}
                "user-agent" if !value.is_empty() => {}
                _ => return Err(Error::new(Kind::InvalidRequest)),
            }
        }
        // A second request contains non-header text or a duplicate/malformed
        // field and is rejected above, even if it ends with CRLF CRLF too.
        Ok(Self {
            host: host.into(),
            port: 443,
        })
    }
}

pub(crate) struct Worker {
    join: Option<JoinHandle<()>>,
    done: mpsc::Receiver<()>,
    failed: bool,
}
impl Worker {
    pub(crate) fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> Result<Self> {
        let (done, receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                // Even a panic cannot manufacture successful settlement: a dropped
                // sender is distinguished from a normal completion message.
                work();
                let _ = done.send(());
            })
            .map_err(|_| Error::new(Kind::ResourceLimit))?;
        Ok(Self {
            join: Some(join),
            done: receiver,
            failed: false,
        })
    }
    fn settle(&mut self, deadline: Instant) -> Result<()> {
        if self.failed {
            return Err(Error::new(Kind::Unsettled));
        }
        if self.join.is_none() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.done.recv_timeout(remaining) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = self.join.take().unwrap().join();
                self.failed = true;
                return Err(Error::new(Kind::Unsettled));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(Error::new(Kind::Unsettled)),
        }
        self.join.take().unwrap().join().map_err(|_| {
            self.failed = true;
            Error::new(Kind::Unsettled)
        })
    }
}

#[derive(Default)]
pub(crate) struct Stats {
    accepted: AtomicU32,
    active: AtomicUsize,
    completed: AtomicU32,
    rejected: AtomicU32,
    pub(crate) dns_jobs: AtomicU32,
    pub(crate) active_dns: AtomicUsize,
    pub(crate) max_dns: AtomicUsize,
    upstreams: AtomicU32,
    max_active: AtomicUsize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProxySnapshot {
    pub accepted: u32,
    pub completed: u32,
    pub rejected: u32,
    pub active_connections: usize,
    pub maximum_active_connections: usize,
    pub dns_jobs: u32,
    pub active_dns_jobs: usize,
    pub maximum_active_dns_jobs: usize,
    pub upstream_connections: u32,
    pub charged_bytes: u64,
    pub cancellation_requested: bool,
}
pub(crate) struct Runtime {
    pub(crate) profile: ValidatedNetworkProfile,
    binding: ConnectBinding,
    pub(crate) host: HostNetworkAnchor,
    authority: Arc<dyn ConnectAuthority>,
    witnesses: Mutex<Option<Arc<ListenerWitnesses>>>,
    cancelled: AtomicBool,
    admission_ready: AtomicBool,
    authority_until: AtomicU64,
    failure: Mutex<Option<Kind>>,
    sockets: Mutex<BTreeMap<u64, RawFd>>,
    socket_serial: AtomicU64,
    pub(crate) dns_serial: AtomicU64,
    pub(crate) resolver_ready: AtomicUsize,
    pub(crate) stats: Stats,
    remaining_bytes: AtomicU64,
}
fn boottime_ms() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) } != 0 {
        return u64::MAX;
    }
    (now.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add(now.tv_nsec as u64 / 1_000_000)
}
impl Runtime {
    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub(crate) fn check_cancelled(&self) -> Result<()> {
        if self.cancelled() {
            Err(Error::new(Kind::Cancelled))
        } else {
            Ok(())
        }
    }
    pub(crate) fn check(&self) -> Result<()> {
        self.check_cancelled()?;
        if boottime_ms() >= self.authority_until.load(Ordering::Acquire) {
            return Err(Error::new(Kind::AuthorityDenied));
        }
        self.check_guest()
    }
    fn check_guest(&self) -> Result<()> {
        self.witnesses
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| Error::new(Kind::Unsettled))?
            .process
            .verify_live()
    }
    fn keep_admitted(&self) -> bool {
        if self.check().is_ok() {
            true
        } else {
            self.cancel();
            false
        }
    }
    fn grant(&self) -> Result<HostIpv4DenySet> {
        self.check_cancelled()?;
        let answer = self.authority.validate(&self.binding);
        self.check_cancelled()?;
        match answer {
            Ok(host) => Ok(host),
            Err(_) => {
                self.fail(Kind::AuthorityDenied);
                Err(Error::new(Kind::AuthorityDenied))
            }
        }
    }
    pub(crate) fn fail(&self, kind: Kind) {
        let mut failure = self.failure.lock().unwrap();
        if failure.is_none() {
            *failure = Some(kind);
        }
        drop(failure);
        self.cancel();
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        // Each owning stream removes its raw FD under this SAME mutex before
        // close. No shutdown can target a descriptor reused by another owner.
        for &fd in self.sockets.lock().unwrap().values() {
            unsafe {
                libc::shutdown(fd, libc::SHUT_RDWR);
            }
        }
    }
    fn snapshot(&self) -> ProxySnapshot {
        ProxySnapshot {
            accepted: self.stats.accepted.load(Ordering::Acquire),
            completed: self.stats.completed.load(Ordering::Acquire),
            rejected: self.stats.rejected.load(Ordering::Acquire),
            active_connections: self.stats.active.load(Ordering::Acquire),
            maximum_active_connections: self.stats.max_active.load(Ordering::Acquire),
            dns_jobs: self.stats.dns_jobs.load(Ordering::Acquire),
            active_dns_jobs: self.stats.active_dns.load(Ordering::Acquire),
            maximum_active_dns_jobs: self.stats.max_dns.load(Ordering::Acquire),
            upstream_connections: self.stats.upstreams.load(Ordering::Acquire),
            charged_bytes: self.profile.budgets().max_total_bytes
                - self.remaining_bytes.load(Ordering::Acquire),
            cancellation_requested: self.cancelled(),
        }
    }
}

pub struct OwnedConnectProxy {
    shared: Arc<Runtime>,
    workers: Vec<Worker>,
    ready: Option<mpsc::Receiver<Result<()>>>,
    settled: bool,
}
impl OwnedConnectProxy {
    /// Call before child ACK. A timeout leaves this owner and all unsettled
    /// workers intact; it does not release permission or yield a receipt.
    pub fn await_ready(&mut self, timeout: Duration) -> Result<()> {
        let ready = self
            .ready
            .as_ref()
            .ok_or_else(|| Error::new(Kind::InvalidConfiguration))?;
        match ready.recv_timeout(timeout) {
            Ok(result) => {
                self.ready.take();
                result?;
                self.shared.check()
            }
            Err(_) => {
                self.shared.cancel();
                Err(Error::new(Kind::Deadline))
            }
        }
    }
    pub fn snapshot(&self) -> ProxySnapshot {
        self.shared.snapshot()
    }
    pub fn cancel(&self) {
        self.shared.cancel();
    }
    pub(crate) fn settle(&mut self, timeout: Duration) -> Result<ConnectedSettlement> {
        if self.settled {
            return Err(Error::new(Kind::InvalidConfiguration));
        }
        self.shared.cancel();
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::new(Kind::Deadline))?;
        for worker in &mut self.workers {
            worker.settle(deadline)?;
        }
        let snapshot = self.shared.snapshot();
        if snapshot.active_connections != 0
            || snapshot.active_dns_jobs != 0
            || !self.shared.sockets.lock().unwrap().is_empty()
        {
            return Err(Error::new(Kind::Unsettled));
        }
        self.settled = true;
        // Network-namespace handles are resource owners too. Drop them before
        // yielding settlement; the retained audit owner contains no such FD.
        self.shared.witnesses.lock().unwrap().take();
        Ok(ConnectedSettlement {
            binding: self.shared.binding.clone(),
            snapshot,
            _owner: Arc::clone(&self.shared),
        })
    }
}
impl Drop for OwnedConnectProxy {
    fn drop(&mut self) {
        self.shared.cancel();
    }
}
/// Only actual joined workers and actual resolver child reaping create this.
/// The retained owner prevents a serialized audit snapshot becoming a witness.
pub struct ConnectedSettlement {
    binding: ConnectBinding,
    snapshot: ProxySnapshot,
    _owner: Arc<Runtime>,
}
impl ConnectedSettlement {
    pub fn binding(&self) -> &ConnectBinding {
        &self.binding
    }
    pub fn snapshot(&self) -> &ProxySnapshot {
        &self.snapshot
    }
}

pub(crate) fn start(
    profile: ValidatedNetworkProfile,
    admission: ListenerAdmission,
    host: HostNetworkAnchor,
    authority: Arc<dyn ConnectAuthority>,
    resolver: ResolverProgram,
) -> Result<OwnedConnectProxy> {
    let binding = ConnectBinding::for_profile(&profile)?;
    if binding != admission.binding || host.check_current().is_err() {
        return Err(Error::new(Kind::BindingMismatch));
    }
    admission.witnesses.process.verify_live()?;
    let bytes = profile.budgets().max_total_bytes;
    let listener_cookie = admission.witnesses.cookie;
    let shared = Arc::new(Runtime {
        profile,
        binding,
        host,
        authority,
        witnesses: Mutex::new(Some(admission.witnesses)),
        cancelled: AtomicBool::new(false),
        admission_ready: AtomicBool::new(false),
        authority_until: AtomicU64::new(0),
        failure: Mutex::new(None),
        sockets: Mutex::new(BTreeMap::new()),
        socket_serial: AtomicU64::new(1),
        dns_serial: AtomicU64::new(1),
        resolver_ready: AtomicUsize::new(0),
        stats: Stats::default(),
        remaining_bytes: AtomicU64::new(bytes),
    });
    let (ready, receiver) = mpsc::sync_channel(1);
    let mut owner = OwnedConnectProxy {
        shared: Arc::clone(&shared),
        workers: Vec::new(),
        ready: Some(receiver),
        settled: false,
    };
    let (dns, dns_workers) = ResolverPool::start(&shared, resolver)?;
    owner.workers.extend(dns_workers);
    let dns = Arc::new(dns);
    let capacity = usize::from(shared.profile.budgets().max_connections);
    let (queue, incoming) = mpsc::sync_channel::<Connection>(capacity);
    let incoming = Arc::new(Mutex::new(incoming));
    for _ in 0..capacity {
        let shared = Arc::clone(&shared);
        let incoming = Arc::clone(&incoming);
        let dns = Arc::clone(&dns);
        owner
            .workers
            .push(Worker::spawn("rudder-connect-relay", move || {
                while !shared.cancelled() {
                    let result = incoming
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_millis(20));
                    let mut connection = match result {
                        Ok(connection) => connection,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    match serve(&mut connection, &shared, &dns) {
                        Ok(()) => {
                            shared.stats.completed.fetch_add(1, Ordering::AcqRel);
                        }
                        Err(_) => {
                            shared.stats.rejected.fetch_add(1, Ordering::AcqRel);
                        }
                    }
                }
                // On cancel, queued streams close before their active permits drop.
                while incoming.lock().unwrap().try_recv().is_ok() {}
            })?);
    }
    let monitor = Arc::clone(&shared);
    owner
        .workers
        .push(Worker::spawn("rudder-connect-grant", move || {
            let deadline = Instant::now()
                + Duration::from_millis(u64::from(monitor.profile.budgets().connect_timeout_ms));
            loop {
                if monitor.cancelled() {
                    let _ = ready.try_send(Err(Error::new(Kind::Cancelled)));
                    return;
                }
                if monitor.grant().is_err() {
                    let _ = ready.try_send(Err(Error::new(Kind::AuthorityDenied)));
                    return;
                }
                if Instant::now() >= deadline {
                    monitor.fail(Kind::Deadline);
                    let _ = ready.try_send(Err(Error::new(Kind::Deadline)));
                    return;
                }
                monitor
                    .authority_until
                    .store(boottime_ms().saturating_add(500), Ordering::Release);
                if monitor.resolver_ready.load(Ordering::Acquire)
                    == usize::from(monitor.profile.budgets().max_dns_jobs)
                {
                    break;
                }
                if Instant::now() >= deadline {
                    monitor.fail(Kind::ResolverFailed);
                    let _ = ready.try_send(Err(Error::new(Kind::ResolverFailed)));
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            if Instant::now() >= deadline {
                monitor.fail(Kind::Deadline);
                let _ = ready.try_send(Err(Error::new(Kind::Deadline)));
                return;
            }
            if monitor.check_guest().is_err() {
                monitor.fail(Kind::InvalidProcess);
                let _ = ready.try_send(Err(Error::new(Kind::InvalidProcess)));
                return;
            }
            monitor.admission_ready.store(true, Ordering::Release);
            let _ = ready.try_send(Ok(()));
            while !monitor.cancelled() {
                thread::sleep(Duration::from_millis(50));
                if monitor.check_guest().is_err() {
                    monitor.cancel();
                    break;
                }
                if monitor.grant().is_err() {
                    break;
                }
                monitor
                    .authority_until
                    .store(boottime_ms().saturating_add(500), Ordering::Release);
            }
        })?);
    let accept = Arc::clone(&shared);
    owner
        .workers
        .push(Worker::spawn("rudder-connect-accept", move || {
            // Merely retaining a listening socket is not admission. No accept call
            // happens before the actual authority and all DNS workers are ready.
            while !accept.cancelled() && !accept.admission_ready.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
            let listener = admission.listener;
            while accept.keep_admitted() {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if accept.stats.accepted.load(Ordering::Acquire) >= MAX_LIFETIME_CONNECTIONS
                        {
                            drop(stream);
                            accept.fail(Kind::ResourceLimit);
                            break;
                        }
                        accept.stats.accepted.fetch_add(1, Ordering::AcqRel);
                        if accept.stats.active.load(Ordering::Acquire) >= capacity {
                            drop(stream);
                            accept.stats.rejected.fetch_add(1, Ordering::AcqRel);
                            continue;
                        }
                        if socket::set_nonblocking_cloexec(stream.as_raw_fd()).is_err()
                            || socket::cookie(stream.as_raw_fd()).ok() != Some(listener_cookie)
                        {
                            drop(stream);
                            accept.fail(Kind::InvalidListener);
                            break;
                        }
                        let active = accept.stats.active.fetch_add(1, Ordering::AcqRel) + 1;
                        accept.stats.max_active.fetch_max(active, Ordering::AcqRel);
                        let connection = Connection {
                            stream: TrackedStream::new(stream, &accept),
                            _permit: ActivePermit(Arc::clone(&accept)),
                            accepted: Instant::now(),
                        };
                        if queue.try_send(connection).is_err() {
                            accept.stats.rejected.fetch_add(1, Ordering::AcqRel);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => {
                        accept.fail(Kind::Io);
                        break;
                    }
                }
            }
            // listener dropped here; witnesses stay with all still-running workers.
            // The accept worker is also the independent expiry watchdog when an
            // authority callback stalls: close every admitted socket immediately.
            accept.cancel();
        })?);
    Ok(owner)
}

struct TrackedStream {
    stream: TcpStream,
    shared: Arc<Runtime>,
    serial: u64,
}
impl TrackedStream {
    fn new(stream: TcpStream, shared: &Arc<Runtime>) -> Self {
        let serial = shared.socket_serial.fetch_add(1, Ordering::AcqRel);
        let mut sockets = shared.sockets.lock().unwrap();
        sockets.insert(serial, stream.as_raw_fd());
        if shared.cancelled() {
            unsafe {
                libc::shutdown(stream.as_raw_fd(), libc::SHUT_RDWR);
            }
        }
        drop(sockets);
        Self {
            stream,
            shared: Arc::clone(shared),
            serial,
        }
    }
}
impl Drop for TrackedStream {
    fn drop(&mut self) {
        let mut sockets = self.shared.sockets.lock().unwrap();
        sockets.remove(&self.serial);
        unsafe {
            libc::shutdown(self.stream.as_raw_fd(), libc::SHUT_RDWR);
        }
        // The stream itself closes immediately after this Drop, while no raw
        // registry entry can survive and later target a reused descriptor.
    }
}
struct ActivePermit(Arc<Runtime>);
impl Drop for ActivePermit {
    fn drop(&mut self) {
        self.0.stats.active.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Connection {
    stream: TrackedStream,
    _permit: ActivePermit,
    accepted: Instant,
}

struct Meter<'a> {
    shared: &'a Runtime,
    used: u64,
}
impl Meter<'_> {
    fn reserve(&self, maximum: usize) -> Result<usize> {
        let maximum = (maximum as u64).min(
            self.shared
                .profile
                .budgets()
                .max_bytes_per_connection
                .saturating_sub(self.used),
        );
        let mut remaining = self.shared.remaining_bytes.load(Ordering::Acquire);
        loop {
            let size = maximum.min(remaining);
            if size == 0 {
                return Err(Error::new(Kind::ResourceLimit));
            }
            match self.shared.remaining_bytes.compare_exchange_weak(
                remaining,
                remaining - size,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(size as usize),
                Err(next) => remaining = next,
            }
        }
    }
    fn read(&mut self, socket: &mut TcpStream, target: &mut [u8]) -> Result<Option<usize>> {
        let reserved = self.reserve(target.len())?;
        let result = socket.read(&mut target[..reserved]);
        let used = result.as_ref().copied().unwrap_or(0);
        self.shared
            .remaining_bytes
            .fetch_add((reserved - used) as u64, Ordering::AcqRel);
        self.used += used as u64;
        match result {
            Ok(count) => Ok(Some(count)),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(_) => Err(Error::new(Kind::Io)),
        }
    }
    fn own_write(&mut self, socket: &mut TcpStream, bytes: &[u8]) -> Result<Option<usize>> {
        let reserved = self.reserve(bytes.len())?;
        let result = socket.write(&bytes[..reserved]);
        let used = result.as_ref().copied().unwrap_or(0);
        self.shared
            .remaining_bytes
            .fetch_add((reserved - used) as u64, Ordering::AcqRel);
        self.used += used as u64;
        match result {
            Ok(0) => Err(Error::new(Kind::Io)),
            Ok(count) => Ok(Some(count)),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(_) => Err(Error::new(Kind::Io)),
        }
    }
}
fn deadlines(shared: &Runtime, total: Instant, idle: Instant) -> Result<()> {
    shared.check()?;
    if Instant::now() >= total
        || Instant::now().duration_since(idle)
            >= Duration::from_millis(u64::from(shared.profile.budgets().idle_timeout_ms))
    {
        return Err(Error::new(Kind::Deadline));
    }
    Ok(())
}
fn serve(connection: &mut Connection, shared: &Arc<Runtime>, dns: &ResolverPool) -> Result<()> {
    let budget = shared.profile.budgets();
    let total = connection.accepted + Duration::from_millis(u64::from(budget.total_timeout_ms));
    let connect_deadline = (connection.accepted
        + Duration::from_millis(u64::from(budget.connect_timeout_ms)))
    .min(total);
    let mut idle = connection.accepted;
    let mut buffer = vec![0u8; budget.max_buffer_bytes as usize];
    let mut meter = Meter { shared, used: 0 };
    let mut length = 0;
    let request = loop {
        deadlines(shared, total, idle)?;
        if length == budget.max_request_bytes as usize {
            return Err(Error::new(Kind::InvalidRequest));
        }
        match meter.read(
            &mut connection.stream.stream,
            &mut buffer[length..budget.max_request_bytes as usize],
        )? {
            Some(0) => return Err(Error::new(Kind::InvalidRequest)),
            Some(count) => {
                length += count;
                idle = Instant::now();
                if let Some(end) = buffer[..length]
                    .windows(4)
                    .position(|bytes| bytes == b"\r\n\r\n")
                {
                    if end + 4 != length {
                        return Err(Error::new(Kind::InvalidRequest));
                    }
                    break ConnectRequest::parse(
                        &buffer[..length],
                        budget.max_request_bytes as usize,
                    )?;
                }
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    };
    if !shared.profile.policy().is_some_and(|policy| {
        policy
            .origins
            .iter()
            .any(|origin| origin.host == request.host && origin.port == request.port)
    }) {
        return Err(Error::new(Kind::DestinationDenied));
    }
    shared.grant()?;
    let network_deadline =
        connect_deadline.min(idle + Duration::from_millis(u64::from(budget.idle_timeout_ms)));
    let addresses = dns.resolve(&request.host, network_deadline, shared)?;
    let host = shared.grant()?;
    validate_answers(&shared.profile, &request, &addresses, &host)?;
    let mut upstream = None;
    for address in addresses {
        deadlines(shared, total, idle)?;
        let host = shared.grant()?;
        shared
            .profile
            .check_destination(&request.host, request.port, IpAddr::V4(address), &host)
            .map_err(|_| Error::new(Kind::DestinationDenied))?;
        if let Ok(stream) = dial(address, request.port, network_deadline, shared) {
            let host = shared.grant()?;
            shared
                .profile
                .check_destination(&request.host, request.port, IpAddr::V4(address), &host)
                .map_err(|_| Error::new(Kind::DestinationDenied))?;
            upstream = Some(stream);
            break;
        }
    }
    let mut upstream = upstream.ok_or_else(|| Error::new(Kind::ConnectFailed))?;
    shared.stats.upstreams.fetch_add(1, Ordering::AcqRel);
    let response = b"HTTP/1.1 200 Connection Established\r\n\r\n";
    let mut written = 0;
    while written < response.len() {
        deadlines(shared, total, idle)?;
        match meter.own_write(&mut connection.stream.stream, &response[written..])? {
            Some(count) => {
                written += count;
                idle = Instant::now();
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    }
    relay_pair(
        &mut connection.stream.stream,
        &mut upstream.stream,
        &mut buffer,
        &mut meter,
        total,
        idle,
    )
}
fn validate_answers(
    profile: &ValidatedNetworkProfile,
    request: &ConnectRequest,
    addresses: &[Ipv4Addr],
    host: &HostIpv4DenySet,
) -> Result<()> {
    if addresses.is_empty()
        || addresses.len() > super::super::MAX_ORIGIN_ADDRESSES
        || !addresses.windows(2).all(|pair| pair[0] < pair[1])
    {
        return Err(Error::new(Kind::DestinationDenied));
    }
    // Reject the ENTIRE result before any dial if any answer is forbidden,
    // unpinned, repeated or oversized. Resolution never expands permission.
    for address in addresses {
        profile
            .check_destination(&request.host, request.port, IpAddr::V4(*address), host)
            .map_err(|_| Error::new(Kind::DestinationDenied))?;
    }
    Ok(())
}
fn dial(
    address: Ipv4Addr,
    port: u16,
    deadline: Instant,
    shared: &Arc<Runtime>,
) -> Result<TrackedStream> {
    shared.check()?;
    if Instant::now() >= deadline {
        return Err(Error::new(Kind::Deadline));
    }
    shared.host.check_current()?;
    let stream = TcpStream::from(socket::new_tcp_socket()?);
    shared.host.check_socket(&stream)?;
    let stream = TrackedStream::new(stream, shared);
    let target = libc::sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(address.octets()),
        },
        sin_zero: [0; 8],
    };
    shared.check()?;
    if Instant::now() >= deadline {
        return Err(Error::new(Kind::Deadline));
    }
    let result = unsafe {
        libc::connect(
            stream.stream.as_raw_fd(),
            (&target as *const libc::sockaddr_in).cast(),
            std::mem::size_of_val(&target) as libc::socklen_t,
        )
    };
    if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINPROGRESS) {
        return Err(Error::new(Kind::ConnectFailed));
    }
    if result != 0 {
        loop {
            shared.check()?;
            if Instant::now() >= deadline {
                return Err(Error::new(Kind::Deadline));
            }
            let mut fd = libc::pollfd {
                fd: stream.stream.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut fd, 1, 10) };
            if result < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return Err(Error::new(Kind::ConnectFailed));
            }
            if result > 0 {
                if socket::option_i32(stream.stream.as_raw_fd(), libc::SO_ERROR)? != 0 {
                    return Err(Error::new(Kind::ConnectFailed));
                }
                break;
            }
        }
    }
    shared.host.check_socket(&stream.stream)?;
    if stream.stream.peer_addr().ok() != Some(SocketAddr::from((address, port))) {
        return Err(Error::new(Kind::ConnectFailed));
    }
    Ok(stream)
}

#[derive(Default)]
struct Direction {
    start: usize,
    end: usize,
    eof: bool,
    shutdown: bool,
}
fn relay_pair(
    guest: &mut TcpStream,
    upstream: &mut TcpStream,
    buffer: &mut [u8],
    meter: &mut Meter<'_>,
    total: Instant,
    mut idle: Instant,
) -> Result<()> {
    let middle = buffer.len() / 2;
    let (out, input) = buffer.split_at_mut(middle);
    let mut g = Direction::default();
    let mut u = Direction::default();
    loop {
        deadlines(meter.shared, total, idle)?;
        let mut progress = false;
        for (source, direction, target) in [
            (&mut *guest, &mut g, &mut *out),
            (&mut *upstream, &mut u, &mut *input),
        ] {
            if direction.start == direction.end {
                direction.start = 0;
                direction.end = 0;
            }
            if !direction.eof && direction.end < target.len() {
                if let Some(count) = meter.read(source, &mut target[direction.end..])? {
                    if count == 0 {
                        direction.eof = true;
                    } else {
                        direction.end += count;
                    }
                    progress = true;
                }
            }
        }
        for (target, direction, bytes) in [
            (&mut *upstream, &mut g, &*out),
            (&mut *guest, &mut u, &*input),
        ] {
            if direction.start < direction.end {
                match target.write(&bytes[direction.start..direction.end]) {
                    Ok(0) => return Err(Error::new(Kind::Io)),
                    Ok(count) => {
                        direction.start += count;
                        progress = true;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return Err(Error::new(Kind::Io)),
                }
            }
            if direction.eof && direction.start == direction.end && !direction.shutdown {
                unsafe {
                    libc::shutdown(target.as_raw_fd(), libc::SHUT_WR);
                }
                direction.shutdown = true;
                progress = true;
            }
        }
        if g.shutdown && u.shutdown {
            return Ok(());
        }
        if progress {
            idle = Instant::now();
        } else {
            let mut fds = [
                libc::pollfd {
                    fd: guest.as_raw_fd(),
                    events: (if !g.eof && g.end < out.len() {
                        libc::POLLIN
                    } else {
                        0
                    }) | (if u.start < u.end { libc::POLLOUT } else { 0 }),
                    revents: 0,
                },
                libc::pollfd {
                    fd: upstream.as_raw_fd(),
                    events: (if !u.eof && u.end < input.len() {
                        libc::POLLIN
                    } else {
                        0
                    }) | (if g.start < g.end { libc::POLLOUT } else { 0 }),
                    revents: 0,
                },
            ];
            unsafe {
                libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 10);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::Shutdown,
        process::{Child, Command, Stdio},
    };
    struct TestAuthority;
    impl ConnectAuthority for TestAuthority {
        fn validate(&self, _: &ConnectBinding) -> Result<HostIpv4DenySet> {
            HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::LOCALHOST])
                .map_err(|_| Error::new(Kind::AuthorityDenied))
        }
    }
    struct Fixture {
        shared: Arc<Runtime>,
        child: Child,
    }
    impl Fixture {
        fn new() -> Self {
            let mut child = Command::new("/bin/sleep")
                .arg("20")
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let profile = super::super::qualification::test_profile();
            let bytes = profile.budgets().max_total_bytes;
            let shared = Arc::new(Runtime {
                binding: ConnectBinding::for_profile(&profile).unwrap(),
                profile,
                host: HostNetworkAnchor::capture().unwrap(),
                authority: Arc::new(TestAuthority),
                witnesses: Mutex::new(Some(socket::test_witness(&mut child))),
                cancelled: AtomicBool::new(false),
                admission_ready: AtomicBool::new(true),
                authority_until: AtomicU64::new(boottime_ms() + 60000),
                failure: Mutex::new(None),
                sockets: Mutex::new(BTreeMap::new()),
                socket_serial: AtomicU64::new(1),
                dns_serial: AtomicU64::new(1),
                resolver_ready: AtomicUsize::new(0),
                stats: Stats::default(),
                remaining_bytes: AtomicU64::new(bytes),
            });
            Self { shared, child }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.shared.cancel();
            // This exact Child remains unreaped until here, reserving its PID.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    fn pair() -> (TcpStream, TcpStream) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let server = listener.accept().unwrap().0;
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        server.set_nonblocking(true).unwrap();
        (client, server)
    }
    #[test]
    fn strict_connect_parser_rejects_smuggling_and_credential_headers() {
        let valid = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nProxy-Connection: Keep-Alive\r\n\r\n";
        assert_eq!(
            ConnectRequest::parse(valid, 4096).unwrap(),
            ConnectRequest {
                host: "example.com".into(),
                port: 443
            }
        );
        for request in [
            "GET https://example.com/ HTTP/1.1\r\n\r\n", "CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n",
            "CONNECT user@example.com:443 HTTP/1.1\r\n\r\n", "CONNECT example.com:0443 HTTP/1.1\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\nHost: evil.com:443\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\nProxy-Authorization: secret\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nHost: example.com:443\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\n\r\nCONNECT example.com:443 HTTP/1.1\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\n\r\nUser-Agent: trailing\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\n\n", "CONNECT Example.com:443 HTTP/1.1\r\n\r\n",
        ] { assert!(ConnectRequest::parse(request.as_bytes(), 4096).is_err(), "unexpected acceptance"); }
        assert!(ConnectRequest::parse(valid, valid.len() - 1).is_err());
    }
    #[test]
    fn actual_relay_drains_half_close_and_preserves_reverse_response() {
        let fixture = Fixture::new();
        let (mut guest, mut guest_proxy) = pair();
        let (mut backend, mut upstream_proxy) = pair();
        let shared = Arc::clone(&fixture.shared);
        let relay = thread::spawn(move || {
            let mut meter = Meter {
                shared: &shared,
                used: 0,
            };
            relay_pair(
                &mut guest_proxy,
                &mut upstream_proxy,
                &mut [0u8; 4096],
                &mut meter,
                Instant::now() + Duration::from_secs(2),
                Instant::now(),
            )
            .unwrap();
            meter.used
        });
        let origin = thread::spawn(move || {
            let mut received = Vec::new();
            backend.read_to_end(&mut received).unwrap();
            assert_eq!(received, b"request-before-half-close");
            backend.write_all(b"response-after-request-eof").unwrap();
            backend.shutdown(Shutdown::Write).unwrap();
        });
        guest.write_all(b"request-before-half-close").unwrap();
        guest.shutdown(Shutdown::Write).unwrap();
        let mut received = Vec::new();
        guest.read_to_end(&mut received).unwrap();
        assert_eq!(received, b"response-after-request-eof");
        origin.join().unwrap();
        assert_eq!(relay.join().unwrap(), 51);
    }
    #[test]
    fn actual_socket_reads_never_exceed_shared_or_per_connection_budget() {
        let fixture = Fixture::new();
        let (mut client, mut proxy) = pair();
        client.write_all(b"1234567890").unwrap();
        fixture.shared.remaining_bytes.store(5, Ordering::Release);
        let mut meter = Meter {
            shared: &fixture.shared,
            used: fixture.shared.profile.budgets().max_bytes_per_connection - 3,
        };
        let mut bytes = [0u8; 16];
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(count) = meter.read(&mut proxy, &mut bytes).unwrap() {
                assert_eq!(count, 3);
                break;
            }
            assert!(Instant::now() < deadline);
        }
        assert_eq!(fixture.shared.remaining_bytes.load(Ordering::Acquire), 2);
        assert_eq!(
            meter.read(&mut proxy, &mut bytes).unwrap_err().kind(),
            Kind::ResourceLimit
        );
        let mut second = Meter {
            shared: &fixture.shared,
            used: 0,
        };
        assert_eq!(second.read(&mut proxy, &mut bytes).unwrap(), Some(2));
        assert_eq!(
            second.read(&mut proxy, &mut bytes).unwrap_err().kind(),
            Kind::ResourceLimit
        );
        assert_eq!(fixture.shared.remaining_bytes.load(Ordering::Acquire), 0);
        // No over-budget byte was removed from the real socket.
        let mut remaining = [0u8; 5];
        proxy.read_exact(&mut remaining).unwrap();
        assert_eq!(&remaining, b"67890");
    }
    #[test]
    fn cancellation_settles_idle_relay_and_actual_stream_owners() {
        let fixture = Fixture::new();
        let (_guest, guest_proxy) = pair();
        let (_origin, upstream_proxy) = pair();
        let shared = Arc::clone(&fixture.shared);
        let mut guest = TrackedStream::new(guest_proxy, &shared);
        let mut origin = TrackedStream::new(upstream_proxy, &shared);
        let relay_shared = Arc::clone(&shared);
        let relay = thread::spawn(move || {
            let mut meter = Meter {
                shared: &relay_shared,
                used: 0,
            };
            relay_pair(
                &mut guest.stream,
                &mut origin.stream,
                &mut [0u8; 4096],
                &mut meter,
                Instant::now() + Duration::from_secs(2),
                Instant::now(),
            )
        });
        shared.cancel();
        assert_eq!(relay.join().unwrap().unwrap_err().kind(), Kind::Cancelled);
        assert!(shared.sockets.lock().unwrap().is_empty());
    }
    #[test]
    fn timeout_keeps_worker_owned_until_actual_return_and_retry_settles() {
        let (release, wait) = mpsc::sync_channel(1);
        let mut worker = Worker::spawn("connect-held-test", move || {
            wait.recv().unwrap();
        })
        .unwrap();
        assert_eq!(
            worker
                .settle(Instant::now() + Duration::from_millis(20))
                .unwrap_err()
                .kind(),
            Kind::Unsettled
        );
        assert!(worker.join.is_some());
        release.send(()).unwrap();
        worker
            .settle(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(worker.join.is_none());
    }
    #[test]
    fn expired_actual_authority_window_denies_traffic_without_refresh() {
        let fixture = Fixture::new();
        fixture.shared.check().unwrap();
        fixture.shared.authority_until.store(0, Ordering::Release);
        assert_eq!(
            fixture.shared.check().unwrap_err().kind(),
            Kind::AuthorityDenied
        );
    }
    #[test]
    fn mixed_rebinding_answers_and_new_host_addresses_reject_before_any_dial() {
        let profile = super::super::qualification::test_profile();
        let request = ConnectRequest {
            host: "example.com".into(),
            port: 443,
        };
        let public = Ipv4Addr::new(93, 184, 216, 34);
        let inventory =
            HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::LOCALHOST]).unwrap();
        validate_answers(&profile, &request, &[public], &inventory).unwrap();
        for answer in [
            vec![],
            vec![Ipv4Addr::new(10, 0, 0, 1), public],
            vec![public, Ipv4Addr::new(93, 184, 216, 35)],
            vec![public, public],
            vec![public; 5],
        ] {
            assert_eq!(
                validate_answers(&profile, &request, &answer, &inventory)
                    .unwrap_err()
                    .kind(),
                Kind::DestinationDenied
            );
        }
        let changed_host = HostIpv4DenySet::from_trusted_host_inventory([public]).unwrap();
        assert!(validate_answers(&profile, &request, &[public], &changed_host).is_err());
    }
    #[test]
    fn close_timeout_keeps_namespace_custody_until_held_callback_really_settles() {
        let fixture = Fixture::new();
        let (release, wait) = mpsc::sync_channel(1);
        let worker = Worker::spawn("connect-held-authority", move || {
            wait.recv().unwrap();
        })
        .unwrap();
        let mut owner = OwnedConnectProxy {
            shared: Arc::clone(&fixture.shared),
            workers: vec![worker],
            ready: None,
            settled: false,
        };
        let error = match owner.close_and_settle(Duration::from_millis(20)) {
            Ok(_) => panic!("early settlement"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), Kind::Unsettled);
        assert!(fixture.shared.witnesses.lock().unwrap().is_some());
        release.send(()).unwrap();
        let settlement = owner.close_and_settle(Duration::from_secs(1)).unwrap();
        assert!(fixture.shared.witnesses.lock().unwrap().is_none());
        assert_eq!(settlement.snapshot().active_connections, 0);
        assert!(owner.close_and_settle(Duration::from_millis(1)).is_err());
    }
    #[test]
    fn queued_dns_permit_is_not_reused_when_requester_times_out() {
        let fixture = Fixture::new();
        let permit = super::super::resolver::DnsPermit::acquire(&fixture.shared).unwrap();
        assert_eq!(fixture.shared.stats.active_dns.load(Ordering::Acquire), 1);
        assert!(super::super::resolver::DnsPermit::acquire(&fixture.shared).is_err());
        // The requesting connection can disappear without owning this permit;
        // the queued/working job retains it until actual completion/reaping.
        fixture.shared.cancel();
        assert!(super::super::resolver::DnsPermit::acquire(&fixture.shared).is_err());
        drop(permit);
        assert_eq!(fixture.shared.stats.active_dns.load(Ordering::Acquire), 0);
        assert_eq!(fixture.shared.stats.max_dns.load(Ordering::Acquire), 1);
    }
    #[test]
    fn expired_grant_watchdog_shuts_down_sockets_even_if_authority_worker_stalls() {
        let fixture = Fixture::new();
        let (mut peer, proxy) = pair();
        let _stream = TrackedStream::new(proxy, &fixture.shared);
        fixture.shared.authority_until.store(0, Ordering::Release);
        assert!(!fixture.shared.keep_admitted());
        assert!(fixture.shared.cancelled());
        assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
    }
    #[test]
    fn expired_dial_deadline_creates_no_upstream_socket_or_effect() {
        let fixture = Fixture::new();
        let result = dial(
            Ipv4Addr::new(93, 184, 216, 34),
            443,
            Instant::now() - Duration::from_millis(1),
            &fixture.shared,
        );
        let error = match result {
            Ok(_) => panic!("expired dial admitted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), Kind::Deadline);
        assert!(fixture.shared.sockets.lock().unwrap().is_empty());
        assert_eq!(fixture.shared.stats.upstreams.load(Ordering::Acquire), 0);
    }
}
