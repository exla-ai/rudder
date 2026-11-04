use super::{
    resolver::ResolverPool,
    socket::{self, HostNetworkAnchor, ListenerAdmission, ListenerWitnesses},
    ConnectAuthority, ConnectBinding, ConnectQuery, ConnectValidation,
    ConnectError as Error, ConnectErrorKind as Kind,
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

type WorkerPanic = Box<dyn std::any::Any + Send + 'static>;
struct JoinState {
    worker: Option<JoinHandle<()>>,
    joined: bool,
    uncertain: bool,
    panic: Option<WorkerPanic>,
}
struct JoinObserver { _thread:JoinHandle<()>, done:mpsc::Receiver<()>, notified:bool }
pub(crate) struct Worker {
    // Remains Some through observer spawn failure, actual join/TLS blocking and
    // uncertain results. The observer takes only the contained actual handle.
    join: Option<Arc<Mutex<JoinState>>>,
    observer: Option<JoinObserver>,
    failed: bool,
}
impl Worker {
    pub(crate) fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> Result<Self> {
        #[cfg(test)]
        let probe = tests::lifecycle::before_spawn(name)?;
        let state = Arc::new(Mutex::new(JoinState { worker:None, joined:false, uncertain:false, panic:None }));
        let join = thread::Builder::new().name(name.into()).spawn(move || {
            #[cfg(test)]
            tests::lifecycle::install_probe(probe);
            work();
        }).map_err(|_|Error::new(Kind::ResourceLimit))?;
        state.lock().unwrap().worker = Some(join); // No callback can access this new state.
        Ok(Self { join:Some(state), observer:None, failed:false })
    }
    fn launch_observer_with(&mut self, spawn:impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>) -> Result<()> {
        if self.observer.is_some() { return Ok(()); }
        let state = self.join.as_ref().ok_or_else(||Error::new(Kind::Unsettled))?.clone();
        let (done, receiver) = mpsc::sync_channel(1);
        // The actual handle stays in self.join's shared slot if spawn fails or
        // drops this closure. Neither path can detach it or run its panic Drop.
        let work = Box::new(move || {
            let worker = {
                let mut stored = state.lock().unwrap_or_else(|error| {
                    let mut stored = error.into_inner(); stored.uncertain = true; stored
                });
                stored.worker.take()
            };
            if let Some(worker) = worker {
                let result = worker.join(); // Actual OS join, including original worker TLS.
                let mut stored = state.lock().unwrap_or_else(|error| {
                    let mut stored = error.into_inner(); stored.uncertain = true; stored
                });
                if let Err(payload) = result {
                    stored.uncertain = true; // Set BEFORE moving any raw panic payload.
                    stored.panic = Some(payload); // Retain; no user payload Drop here.
                }
                stored.joined = true;
            } else {
                state.lock().unwrap_or_else(|error|error.into_inner()).uncertain = true;
            }
            let _ = done.send(()); // A notification only AFTER the actual original join.
        });
        let observer = spawn(work).map_err(|_|Error::new(Kind::ResourceLimit))?;
        self.observer = Some(JoinObserver { _thread:observer, done:receiver, notified:false });
        Ok(())
    }
    fn settle(&mut self, deadline: Instant) -> Result<()> {
        if self.failed { return Err(Error::new(Kind::Unsettled)); }
        if self.join.is_none() { return Ok(()); }
        self.launch_observer_with(|work|thread::Builder::new().name("rudder-connect-join".into()).spawn(work))?;
        let observer = self.observer.as_mut().unwrap();
        if !observer.notified {
            match observer.done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => observer.notified = true,
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(Error::new(Kind::Unsettled)),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.failed = true; // Retain state/observer; no unknown insert/retry success.
                    return Err(Error::new(Kind::Unsettled));
                }
            }
        }
        if Instant::now() >= deadline { return Err(Error::new(Kind::Unsettled)); }
        {
            let state = self.join.as_ref().unwrap().lock().unwrap_or_else(|error| {
                let mut state = error.into_inner(); state.uncertain = true; state
            });
            if !state.joined || state.uncertain || state.panic.is_some() {
                self.failed = true;
                return Err(Error::new(Kind::Unsettled));
            }
        }
        self.join.take(); // Empty actual-handle slot; observer stays held for audit.
        Ok(())
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
    validation: Mutex<Option<Arc<ConnectValidation>>>,
    failure: Mutex<Option<Kind>>,
    startup_panic: Mutex<Option<WorkerPanic>>,
    sockets: Mutex<BTreeMap<u64, RawFd>>,
    socket_serial: AtomicU64,
    pub(crate) dns_serial: AtomicU64,
    pub(crate) resolver_ready: AtomicUsize,
    pub(crate) stats: Stats,
    remaining_bytes: AtomicU64,
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
    fn validation(&self) -> Result<Arc<ConnectValidation>> {
        self.validation.lock().map_err(|_|Error::new(Kind::Unsettled))?
            .as_ref().cloned().ok_or_else(||Error::new(Kind::AuthorityDenied))
    }
    pub(crate) fn check(&self) -> Result<()> {
        self.check_cancelled()?;
        // Clone under the slot mutex, then run any authority/inventory callback
        // AFTER releasing it. The fixed IO worker owns this Arc until return.
        self.validation()?.check_current()?;
        self.check_cancelled()?;
        self.check_guest()
    }
    fn check_bounds(&self) -> Result<()> {
        self.check_cancelled()?;
        self.validation()?.check_bounds()?; // Callback-free independent watchdog.
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
        if self.check_bounds().is_ok() {
            true
        } else {
            self.cancel();
            false
        }
    }
    fn grant(&self) -> Result<Arc<ConnectValidation>> {
        self.check_cancelled()?;
        let query = match ConnectQuery::begin(&self.binding) {
            Ok(query) => query, Err(error) => { self.fail(Kind::AuthorityDenied); return Err(error); }
        };
        let identity = query.identity();
        // Query time starts BEFORE this potentially blocking callback. Runtime
        // holds no IO/validation mutex while it runs. No clock is restarted.
        let answer = self.authority.clone().validate(query);
        self.check_cancelled()?;
        let result = (|| {
            let validation = answer?;
            if !validation.matches(&self.binding, &identity) {
                return Err(Error::new(Kind::BindingMismatch));
            }
            let host = validation.check_adoption()?;
            // A refreshed inventory also revokes OPEN tunnels if any sealed
            // pin became host/NAT-denied, before adopting the actual result.
            let allowed = self.profile.policy().is_some_and(|policy| {
                policy.origins.iter().all(|origin| origin.ipv4_addresses.iter().all(|address|
                    self.profile.check_destination(&origin.host, origin.port, IpAddr::V4(*address), host).is_ok()))
            });
            if !allowed { return Err(Error::new(Kind::AuthorityDenied)); }
            validation.check_adoption()?;
            self.check_cancelled()?;
            Ok(Arc::new(validation))
        })();
        if result.is_err() { self.fail(Kind::AuthorityDenied); }
        result
    }
    fn adopt_grant(&self, validation: Arc<ConnectValidation>) -> Result<()> {
        let result = (|| {
            validation.check_adoption()?;
            self.check_cancelled()?;
            let previous = {
                let mut current = self.validation.lock().map_err(|_|Error::new(Kind::Unsettled))?;
                self.check_cancelled()?;
                validation.check_adoption_bounds()?; // Syscalls only while locked.
                current.replace(validation)
            };
            drop(previous); // Actual owner destructors also run OUTSIDE the mutex.
            Ok(())
        })();
        if result.is_err() { self.fail(Kind::AuthorityDenied); }
        result
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
        let mut failure = None;
        for worker in &mut self.workers {
            // A sticky panic in one worker must not starve actual cleanup of
            // other workers. Every observer uses the SAME original deadline.
            if let Err(error) = worker.settle(deadline) { failure.get_or_insert(error); }
        }
        if let Some(error) = failure { return Err(error); }
        if self.shared.startup_panic.lock().map_err(|_|Error::new(Kind::Unsettled))?.is_some() {
            return Err(Error::new(Kind::Unsettled)); // Actual panic owner remains retained, never normal settlement.
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
            _workers: std::mem::take(&mut self.workers),
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
    // Actual original workers joined; retain their engine-only join observers.
    _workers: Vec<Worker>,
}
impl ConnectedSettlement {
    pub fn binding(&self) -> &ConnectBinding {
        &self.binding
    }
    pub fn snapshot(&self) -> &ProxySnapshot {
        &self.snapshot
    }
}

/// OFF-APP. Caller retains this slot before entry and on EVERY Err/unwind.
/// None after an early error means no worker was started; Some is actual cleanup
/// ownership, not startup/ACK. Never flatten a failed Some owner into a bare Err.
pub(crate) fn start_retaining(
    profile: ValidatedNetworkProfile,
    admission: ListenerAdmission,
    host: HostNetworkAnchor,
    authority: Arc<dyn ConnectAuthority>,
    resolver: ResolverProgram,
    retained: &mut Option<OwnedConnectProxy>,
) -> Result<()> {
    if retained.is_some() { return Err(Error::new(Kind::InvalidConfiguration)); }
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
        validation: Mutex::new(None),
        failure: Mutex::new(None),
        startup_panic: Mutex::new(None),
        sockets: Mutex::new(BTreeMap::new()),
        socket_serial: AtomicU64::new(1),
        dns_serial: AtomicU64::new(1),
        resolver_ready: AtomicUsize::new(0),
        stats: Stats::default(),
        remaining_bytes: AtomicU64::new(bytes),
    });
    let (ready, receiver) = mpsc::sync_channel(1);
    let capacity = usize::from(shared.profile.budgets().max_connections);
    let worker_capacity = capacity + usize::from(shared.profile.budgets().max_dns_jobs) + 2;
    *retained = Some(OwnedConnectProxy {
        shared: Arc::clone(&shared),
        workers: Vec::with_capacity(worker_capacity),
        ready: Some(receiver),
        settled: false,
    });
    provision_retaining(retained.as_mut().unwrap(), |owner| {
    // ResolverPool must borrow this vector too; a resolver-local temporary Vec
    // would lose partial startup ownership before our outer catch can see it.
    let dns = Arc::new(ResolverPool::start_retaining(&shared, resolver, &mut owner.workers)?);
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
                if monitor.grant().and_then(|validation|monitor.adopt_grant(validation)).is_err() {
                    let _ = ready.try_send(Err(Error::new(Kind::AuthorityDenied)));
                    return;
                }
                if Instant::now() >= deadline {
                    monitor.fail(Kind::Deadline);
                    let _ = ready.try_send(Err(Error::new(Kind::Deadline)));
                    return;
                }
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
                // 200 ms maximum adoption +50 ms cadence +200 ms next adoption
                // fits inside the original 500 ms staleness bound. Scheduling
                // overrun still fences; late delivery never buys fresh time.
                thread::sleep(Duration::from_millis(50));
                if monitor.check_guest().is_err() {
                    monitor.cancel();
                    break;
                }
                if monitor.grant().and_then(|validation|monitor.adopt_grant(validation)).is_err() {
                    break;
                }
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
    Ok(())
    })
}

fn provision_retaining(owner:&mut OwnedConnectProxy, provision:impl FnOnce(&mut OwnedConnectProxy) -> Result<()>) -> Result<()> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||provision(owner)));
    let result = match result {
        Ok(result) => result,
        Err(payload) => {
            // Store before any later failure/drop can run arbitrary panic Drop.
            *owner.shared.startup_panic.lock().unwrap_or_else(|error|error.into_inner()) = Some(payload);
            Err(Error::new(Kind::Unsettled))
        }
    };
    if let Err(error) = result {
        owner.ready.take(); // No failed/partial startup can later produce a readiness ACK.
        owner.shared.fail(error.kind());
    }
    result
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
        self.shared.check()?;
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
        self.shared.check()?;
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
    validate_answers(&shared.profile, &request, &addresses, host.check_current()?)?;
    let mut upstream = None;
    for address in addresses {
        deadlines(shared, total, idle)?;
        let host = shared.grant()?;
        shared
            .profile
            .check_destination(&request.host, request.port, IpAddr::V4(address), host.check_current()?)
            .map_err(|_| Error::new(Kind::DestinationDenied))?;
        if let Ok(stream) = dial(address, request.port, network_deadline, shared) {
            let host = shared.grant()?;
            shared
                .profile
                .check_destination(&request.host, request.port, IpAddr::V4(address), host.check_current()?)
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
                meter.shared.check()?;
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
                meter.shared.check()?;
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
    use super::super::{ConnectDeadline, ConnectValidationGuard, validity::qualifier_deadline};
    struct TestAuthority { fixed:ConnectDeadline }
    impl ConnectValidationGuard for TestAuthority {
        fn check_current(&self) -> Result<()> { Ok(()) }
    }
    impl ConnectAuthority for TestAuthority {
        fn validate(self:Arc<Self>, query:ConnectQuery) -> Result<ConnectValidation> {
            let host = HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::LOCALHOST])
                .map_err(|_|Error::new(Kind::AuthorityDenied))?;
            ConnectValidation::new(query, host, self.fixed, self.fixed, self.fixed, self)
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
                authority: Arc::new(TestAuthority { fixed:qualifier_deadline(60_000) }),
                witnesses: Mutex::new(Some(socket::test_witness(&mut child))),
                cancelled: AtomicBool::new(false),
                admission_ready: AtomicBool::new(true),
                validation: Mutex::new(None),
                failure: Mutex::new(None),
                startup_panic: Mutex::new(None),
                sockets: Mutex::new(BTreeMap::new()),
                socket_serial: AtomicU64::new(1),
                dns_serial: AtomicU64::new(1),
                resolver_ready: AtomicUsize::new(0),
                stats: Stats::default(),
                remaining_bytes: AtomicU64::new(bytes),
            });
            shared.adopt_grant(shared.grant().unwrap()).unwrap();
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
    fn expire_original_grant(fixture: &Fixture) {
        let query = ConnectQuery::begin(&fixture.shared.binding).unwrap();
        let fixed = qualifier_deadline(100);
        let owner = Arc::new(TestAuthority { fixed });
        let host = HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::LOCALHOST]).unwrap();
        let result = Arc::new(ConnectValidation::new(query, host, fixed, fixed, fixed, owner).unwrap());
        fixture.shared.adopt_grant(result.clone()).unwrap();
        let end = Instant::now() + Duration::from_secs(1);
        while result.check_bounds().is_ok() {
            assert!(Instant::now() < end); thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn expired_actual_authority_window_denies_traffic_without_refresh() {
        let fixture = Fixture::new();
        fixture.shared.check().unwrap();
        expire_original_grant(&fixture);
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
        expire_original_grant(&fixture);
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
    #[test]
    fn refreshed_host_deny_closes_open_relay_before_renewal_and_retains_worker_until_settlement() {
        struct ChangingAuthority { addresses:Mutex<Vec<Ipv4Addr>>, fixed:ConnectDeadline }
        impl ConnectValidationGuard for ChangingAuthority {
            fn check_current(&self) -> Result<()> { Ok(()) }
        }
        impl ConnectAuthority for ChangingAuthority {
            fn validate(self:Arc<Self>, query:ConnectQuery) -> Result<ConnectValidation> {
                let host = HostIpv4DenySet::from_trusted_host_inventory(self.addresses.lock().unwrap().clone())
                    .map_err(|_|Error::new(Kind::AuthorityDenied))?;
                ConnectValidation::new(query, host, self.fixed, self.fixed, self.fixed, self)
            }
        }
        let mut fixture = Fixture::new();
        let authority = Arc::new(ChangingAuthority { addresses:Mutex::new(vec![Ipv4Addr::LOCALHOST]), fixed:qualifier_deadline(60_000) });
        Arc::get_mut(&mut fixture.shared).unwrap().authority = authority.clone();
        fixture.shared.adopt_grant(fixture.shared.grant().unwrap()).unwrap();
        let (mut guest, guest_proxy) = pair();
        let (mut origin, upstream_proxy) = pair();
        origin
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        fixture.shared.stats.active.store(1, Ordering::Release);
        let mut connection = Connection {
            stream: TrackedStream::new(guest_proxy, &fixture.shared),
            _permit: ActivePermit(Arc::clone(&fixture.shared)),
            accepted: Instant::now(),
        };
        let mut upstream = TrackedStream::new(upstream_proxy, &fixture.shared);
        let shared = Arc::clone(&fixture.shared);
        let (returned, stopped) = mpsc::sync_channel(1);
        let (release, hold) = mpsc::sync_channel(1);
        let worker = Worker::spawn("connect-revoked-open-relay", move || {
            let mut meter = Meter {
                shared: &shared,
                used: 0,
            };
            let result = relay_pair(
                &mut connection.stream.stream,
                &mut upstream.stream,
                &mut [0u8; 4096],
                &mut meter,
                Instant::now() + Duration::from_secs(3),
                Instant::now(),
            );
            returned.send(result.map_err(|error| error.kind())).unwrap();
            // Retain actual socket/permit owners after logical relay return.
            // A close timeout cannot turn this held worker into settlement.
            hold.recv().unwrap();
            drop(upstream);
            drop(connection);
        })
        .unwrap();
        let mut owner = OwnedConnectProxy {
            shared: Arc::clone(&fixture.shared),
            workers: vec![worker],
            ready: None,
            settled: false,
        };
        guest.write_all(b"before").unwrap();
        let mut before = [0u8; 6];
        origin.read_exact(&mut before).unwrap();
        assert_eq!(&before, b"before");
        origin.write_all(b"reply").unwrap();
        let mut reply = [0u8; 5];
        guest.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"reply");
        *authority.addresses.lock().unwrap() = vec![Ipv4Addr::LOCALHOST, Ipv4Addr::new(93, 184, 216, 34)];
        let previous = fixture.shared.validation().unwrap();
        let refresh = fixture.shared.grant().and_then(|validation|fixture.shared.adopt_grant(validation));
        let cancelled_before_close = fixture.shared.cancelled();
        let validity_after_refresh = fixture.shared.validation().unwrap();
        let _ = guest.write_all(b"AFTER");
        let mut after = [0u8; 5];
        let peer_read = origin.read(&mut after);
        let forwarded_after_revocation = matches!(peer_read, Ok(count) if count > 0);
        let peer_closed = matches!(peer_read, Ok(0))
            || peer_read.as_ref().is_err_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::NotConnected
                )
            });
        // Always settle the fixture before assertions, including the old-code
        // negative control, so a deliberate regression cannot leak its worker.
        let early = owner.close_and_settle(Duration::from_millis(20));
        let still_owned = fixture.shared.witnesses.lock().unwrap().is_some()
            && fixture.shared.stats.active.load(Ordering::Acquire) == 1
            && !fixture.shared.sockets.lock().unwrap().is_empty();
        let relay_result = stopped.recv_timeout(Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
        let settled = owner.close_and_settle(Duration::from_secs(1)).unwrap();
        assert!(
            !forwarded_after_revocation && peer_closed,
            "open tunnel forwarded after its sealed pin became host-denied"
        );
        assert!(cancelled_before_close && refresh.is_err());
        assert!(Arc::ptr_eq(&validity_after_refresh, &previous), "denied refresh published a different owner");
        assert!(matches!(early, Err(error) if error.kind() == Kind::Unsettled));
        assert!(still_owned);
        assert!(matches!(
            relay_result,
            Ok(()) | Err(Kind::Cancelled) | Err(Kind::Io)
        ));
        assert_eq!(settled.snapshot().active_connections, 0);
        assert!(fixture.shared.sockets.lock().unwrap().is_empty());
    }
    include!("validity_relay_tests.rs");
    pub(crate) mod lifecycle {
        use super::*;
        include!("relay_lifecycle_tests.rs");
    }

}
