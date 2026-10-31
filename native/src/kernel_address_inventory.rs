#![cfg(target_os = "linux")]
//! Actual current host-NET kernel IPv4 custody, not a complete host inventory.
//! No Fly outer-VM attestation, public/NAT/ingress coverage, routing-mutation
//! control, Journal grant or model activation is established here. In particular
//! this module never constructs source_connect_authority::TrustedHostInventoryLease.
//! All methods and final FD disposal belong off-App. A blocked caller keeps this
//! actual owner charged until return/join; deadline expiry is not settlement.
use rudder_engine::{execution::network::{linux::{NamespaceFd, NamespaceIdentity, NamespaceKind},
    MAX_HOST_IPV4_ADDRESSES}, fleet_lease::{HostClock, SuspendAwareClock}};
use std::{collections::BTreeSet, fs::{self, File, OpenOptions}, io,
    net::Ipv4Addr, os::{fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    unix::fs::{FileExt, MetadataExt, OpenOptionsExt}},
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH}};

const MAX_SNAPSHOT_MS: i64 = 250;
const NS_PER_MS: u128 = 1_000_000;
const SO_NETNS_COOKIE: i32 = 71;
const ADDRESS_GROUPS: u32 = 1 | 0x10; // RTMGRP_LINK | RTMGRP_IPV4_IFADDR.
const HEADER: usize = 16;
const MAX_PACKET: usize = 32 * 1024;
const MAX_DUMP_BYTES: usize = 1024 * 1024;
const MAX_DUMP_MESSAGES: usize = 1024;
const NLMSG_DONE: u16 = 3;
const NLMSG_OVERRUN: u16 = 4;
const NLM_F_MULTI: u16 = 2;
const NLM_F_DUMP_INTR: u16 = 0x10;
const RTM_NEWADDR: u16 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KernelInventoryError {
    Fenced, Process, Namespace, Boot, Clock, Deadline, Changed, Incomplete, Limit, Io,
}
impl std::fmt::Display for KernelInventoryError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "kernel address inventory failed ({:?})", self)
    }
}
impl std::error::Error for KernelInventoryError {}
type Result<T> = std::result::Result<T, KernelInventoryError>;

#[derive(Clone, Copy)]
struct Sample { wall_ms: i64, boot_ns: u128 }
/// Original absolute bounds DATA only. Copying this cannot mint custody.
#[derive(Clone, Copy)]
pub(crate) struct KernelSnapshotDeadline { wall_ms: i64, boot_ns: u128 }
impl KernelSnapshotDeadline {
    pub(crate) fn wall_ms(self) -> i64 { self.wall_ms }
    pub(crate) fn boottime_ns(self) -> u128 { self.boot_ns }
    fn begin(at: Sample, lifetime: Duration) -> Result<Self> {
        let ms = i64::try_from(lifetime.as_millis()).map_err(|_| KernelInventoryError::Limit)?;
        if !(1..=MAX_SNAPSHOT_MS).contains(&ms) { return Err(KernelInventoryError::Limit); }
        Ok(Self { wall_ms: at.wall_ms.checked_add(ms).ok_or(KernelInventoryError::Clock)?,
            boot_ns: at.boot_ns.checked_add(ms as u128 * NS_PER_MS).ok_or(KernelInventoryError::Clock)? })
    }
    fn check(self, at: Sample) -> Result<()> {
        if at.wall_ms >= self.wall_ms || at.boot_ns >= self.boot_ns { return Err(KernelInventoryError::Deadline); }
        Ok(())
    }
    fn poll_ms(self, at: Sample) -> Result<i32> {
        self.check(at)?;
        let ms = (self.wall_ms - at.wall_ms) as u128;
        let boot_ms = (self.boot_ns - at.boot_ns).div_ceil(NS_PER_MS);
        Ok(ms.min(boot_ms).clamp(1, 10) as i32)
    }
}
struct BootFile { file: File, device: u64, inode: u64, bytes: Vec<u8> }
impl BootFile {
    fn capture() -> Result<Self> {
        let file = OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open("/proc/sys/kernel/random/boot_id").map_err(|_| KernelInventoryError::Boot)?;
        let mut kind: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(file.as_raw_fd(), &mut kind) } != 0 || kind.f_type != 0x9fa0 {
            return Err(KernelInventoryError::Boot);
        }
        let meta = file.metadata().map_err(|_| KernelInventoryError::Boot)?;
        let bytes = boot_bytes(&file)?;
        let value = Self { file, device: meta.dev(), inode: meta.ino(), bytes };
        value.check()?; Ok(value)
    }
    fn check(&self) -> Result<()> {
        let named = fs::symlink_metadata("/proc/sys/kernel/random/boot_id").map_err(|_| KernelInventoryError::Boot)?;
        if !named.is_file() || (named.dev(), named.ino()) != (self.device, self.inode)
            || boot_bytes(&self.file)? != self.bytes { return Err(KernelInventoryError::Boot); }
        Ok(())
    }
}
fn boot_bytes(file: &File) -> Result<Vec<u8>> {
    let mut bytes = [0u8; 65];
    let n = file.read_at(&mut bytes, 0).map_err(|_| KernelInventoryError::Boot)?;
    if n != 37 || bytes[36] != b'\n'
        || std::str::from_utf8(&bytes[..36]).ok().and_then(|s|uuid::Uuid::parse_str(s).ok())
            .is_none_or(|id|id.to_string().as_bytes() != &bytes[..36]) {
        return Err(KernelInventoryError::Boot);
    }
    Ok(bytes[..n].to_vec())
}
fn current_net() -> Result<NamespaceFd> {
    // Known kernel path, never a caller-supplied namespace or serialized inode.
    let file = OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open("/proc/thread-self/ns/net").map_err(|_| KernelInventoryError::Namespace)?;
    NamespaceFd::inspect(file.into(), NamespaceKind::Network).map_err(|_| KernelInventoryError::Namespace)
}
fn socket(family: i32, kind: i32, protocol: i32) -> Result<OwnedFd> {
    let fd = unsafe { libc::socket(family, kind | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, protocol) };
    if fd < 0 { return Err(KernelInventoryError::Io); }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
fn cookie(fd: RawFd) -> Result<u64> {
    let mut value = 0u64; let mut size = std::mem::size_of_val(&value) as libc::socklen_t;
    if unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, SO_NETNS_COOKIE,
        (&mut value as *mut u64).cast(), &mut size) } != 0 || size as usize != 8 || value == 0 {
        return Err(KernelInventoryError::Namespace);
    }
    Ok(value)
}
struct RouteSocket { fd: OwnedFd, port: u32 }
impl RouteSocket {
    fn bind(groups: u32, expected_cookie: u64) -> Result<Self> {
        let fd = socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE)?;
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as u16; address.nl_groups = groups;
        if unsafe { libc::bind(fd.as_raw_fd(), (&address as *const libc::sockaddr_nl).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t) } != 0 {
            return Err(KernelInventoryError::Io);
        }
        let mut size = std::mem::size_of_val(&address) as libc::socklen_t;
        if unsafe { libc::getsockname(fd.as_raw_fd(), (&mut address as *mut libc::sockaddr_nl).cast(), &mut size) } != 0
            || size as usize != std::mem::size_of_val(&address)
            || address.nl_family != libc::AF_NETLINK as u16 || address.nl_pid == 0
            || address.nl_groups != groups || cookie(fd.as_raw_fd())? != expected_cookie {
            return Err(KernelInventoryError::Namespace);
        }
        Ok(Self { fd, port: address.nl_pid })
    }
    fn request_dump(&self, sequence: u32) -> Result<()> {
        let mut request = Vec::from(24u32.to_ne_bytes());
        request.extend(22u16.to_ne_bytes()); request.extend(0x301u16.to_ne_bytes()); // GETADDR / REQUEST|DUMP.
        request.extend(sequence.to_ne_bytes()); request.extend(self.port.to_ne_bytes());
        request.extend([libc::AF_INET as u8, 0, 0, 0, 0, 0, 0, 0]);
        let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() }; kernel.nl_family = libc::AF_NETLINK as u16;
        if unsafe { libc::sendto(self.fd.as_raw_fd(), request.as_ptr().cast(), request.len(), 0,
            (&kernel as *const libc::sockaddr_nl).cast(), std::mem::size_of_val(&kernel) as libc::socklen_t) } != request.len() as isize {
            return Err(KernelInventoryError::Incomplete);
        }
        Ok(())
    }
}
struct Packet { bytes: Vec<u8>, sender: u32 }
fn receive(fd: RawFd) -> Result<Option<Packet>> {
    receive_limit(fd, MAX_PACKET)
}
fn receive_limit(fd: RawFd, limit: usize) -> Result<Option<Packet>> {
    if limit == 0 || limit > MAX_PACKET { return Err(KernelInventoryError::Limit); }
    let mut bytes = vec![0u8; limit];
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    let mut vector = libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: bytes.len() };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_name = (&mut address as *mut libc::sockaddr_nl).cast();
    message.msg_namelen = std::mem::size_of_val(&address) as libc::socklen_t;
    message.msg_iov = &mut vector; message.msg_iovlen = 1;
    let n = unsafe { libc::recvmsg(fd, &mut message, libc::MSG_DONTWAIT) };
    if n < 0 {
        return if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock { Ok(None) }
            else { Err(KernelInventoryError::Incomplete) }; // Includes ENOBUFS and interruptions.
    }
    if n == 0 || n as usize > bytes.len() || message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || message.msg_namelen as usize != std::mem::size_of_val(&address)
        || address.nl_family != libc::AF_NETLINK as u16 || address.nl_pid != 0 {
        return Err(KernelInventoryError::Incomplete);
    }
    bytes.truncate(n as usize);
    // sockaddr_nl.nl_pid authenticates the kernel sender. The nlmsg_pid header
    // commonly contains our destination port ID; it is NOT a sender witness.
    Ok(Some(Packet { bytes, sender: address.nl_pid }))
}
struct Frame<'a> { kind: u16, flags: u16, sequence: u32, payload: &'a [u8] }
fn frames(bytes: &[u8]) -> Result<Vec<Frame<'_>>> {
    let mut at = 0; let mut result = Vec::new();
    while at < bytes.len() {
        let header = bytes.get(at..at + HEADER).ok_or(KernelInventoryError::Incomplete)?;
        let length = u32::from_ne_bytes(header[..4].try_into().unwrap()) as usize;
        if length < HEADER || length > bytes.len() - at { return Err(KernelInventoryError::Incomplete); }
        result.push(Frame { kind: u16::from_ne_bytes(header[4..6].try_into().unwrap()),
            flags: u16::from_ne_bytes(header[6..8].try_into().unwrap()),
            sequence: u32::from_ne_bytes(header[8..12].try_into().unwrap()), payload: &bytes[at + HEADER..at + length] });
        if result.len() > MAX_DUMP_MESSAGES { return Err(KernelInventoryError::Limit); }
        let next = at.checked_add((length + 3) & !3).ok_or(KernelInventoryError::Limit)?;
        at = if next > bytes.len() && at + length == bytes.len() { bytes.len() } else { next };
        if at > bytes.len() { return Err(KernelInventoryError::Incomplete); }
    }
    if result.is_empty() { return Err(KernelInventoryError::Incomplete); }
    Ok(result)
}
fn address(payload: &[u8], addresses: &mut BTreeSet<Ipv4Addr>) -> Result<()> {
    if payload.len() < 8 || payload[0] != libc::AF_INET as u8 || payload[1] > 32
        || u32::from_ne_bytes(payload[4..8].try_into().unwrap()) == 0 {
        return Err(KernelInventoryError::Incomplete);
    }
    let mut at = 8; let mut local = false;
    while at < payload.len() {
        let header = payload.get(at..at + 4).ok_or(KernelInventoryError::Incomplete)?;
        let length = u16::from_ne_bytes(header[..2].try_into().unwrap()) as usize;
        let kind = u16::from_ne_bytes(header[2..4].try_into().unwrap());
        if length < 4 || length > payload.len() - at { return Err(KernelInventoryError::Incomplete); }
        let value = &payload[at + 4..at + length];
        match kind {
            1 | 2 | 4 | 5 | 7 => { // Address/local/peer/broadcast/anycast/multicast: conservatively deny all.
                let octets: [u8; 4] = value.try_into().map_err(|_| KernelInventoryError::Incomplete)?;
                addresses.insert(Ipv4Addr::from(octets)); local |= kind == 1 || kind == 2;
            }
            3 if !value.is_empty() && value.len() <= libc::IFNAMSIZ as usize && value.last() == Some(&0) => {}
            6 if value.len() == 16 => {} // ifa_cacheinfo.
            8 | 9 if value.len() == 4 => {} // Flags/route priority.
            11 if value.len() == 1 => {} // IFA_PROTO.
            _ => return Err(KernelInventoryError::Incomplete), // Unknown/cross-NET attributes have no fallback.
        }
        if addresses.len() > MAX_HOST_IPV4_ADDRESSES { return Err(KernelInventoryError::Limit); }
        at = at.checked_add((length + 3) & !3).ok_or(KernelInventoryError::Limit)?;
        if at > payload.len() { return Err(KernelInventoryError::Incomplete); }
    }
    if !local { return Err(KernelInventoryError::Incomplete); }
    Ok(())
}
fn consume_dump(packet: &Packet, sequence: u32, addresses: &mut BTreeSet<Ipv4Addr>, messages: &mut usize) -> Result<bool> {
    if packet.sender != 0 { return Err(KernelInventoryError::Incomplete); }
    let frames = frames(&packet.bytes)?;
    for (at, frame) in frames.iter().enumerate() {
        *messages = messages.checked_add(1).ok_or(KernelInventoryError::Limit)?;
        if *messages > MAX_DUMP_MESSAGES { return Err(KernelInventoryError::Limit); }
        if frame.sequence != sequence || frame.flags & NLM_F_DUMP_INTR != 0 || frame.flags & NLM_F_MULTI == 0 {
            return Err(KernelInventoryError::Incomplete);
        }
        match frame.kind {
            RTM_NEWADDR => address(frame.payload, addresses)?,
            NLMSG_DONE if at + 1 == frames.len() => {
                if !(frame.payload.is_empty() || frame.payload.len() == 4
                    && i32::from_ne_bytes(frame.payload.try_into().unwrap()) == 0) || addresses.is_empty() {
                    return Err(KernelInventoryError::Incomplete);
                }
                return Ok(true);
            }
            _ => return Err(KernelInventoryError::Incomplete), // ERROR/OVERRUN/unknown never complete a dump.
        }
    }
    Ok(false)
}

struct State { last: Sample, revision: u64, sequence: u32 }
/// No raw-address/bool/JSON constructor. This witnesses only this process's
/// actual current NET. A trusted bootstrap must separately bind it as Fly's VM
/// outer NET and retain app/Machine/ingress mutation custody.
#[must_use = "retain through every snapshot user and actual callback settlement"]
pub(crate) struct KernelAddressCustody {
    process: u32, pidfd: OwnedFd, namespace: NamespaceFd, boot: BootFile,
    probe: OwnedFd, net_cookie: u64, monitor: RouteSocket, clock: HostClock,
    state: Mutex<State>, fenced: AtomicBool,
}
pub(crate) struct KernelIpv4Snapshot {
    owner: Arc<KernelAddressCustody>, addresses: BTreeSet<Ipv4Addr>, revision: u64,
    deadline: KernelSnapshotDeadline,
}
impl KernelAddressCustody {
    /// OFF-APP, in the trusted host bootstrap before child namespace entry.
    /// No external packets, namespace mutation or provider operation occurs.
    pub(crate) fn capture_current() -> Result<Arc<Self>> {
        let clock = HostClock::new().map_err(|_| KernelInventoryError::Clock)?;
        let boot_ns = clock.read().map_err(|_| KernelInventoryError::Clock)?.nanoseconds();
        let wall_ms = wall()?; let start = Sample { wall_ms, boot_ns };
        let deadline = KernelSnapshotDeadline::begin(start, Duration::from_millis(MAX_SNAPSHOT_MS as u64))?;
        let process = std::process::id();
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, process, 0) } as i32;
        if pidfd < 0 { return Err(KernelInventoryError::Process); }
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
        let flags = unsafe { libc::fcntl(pidfd.as_raw_fd(), libc::F_GETFD) };
        if flags < 0 || flags & libc::FD_CLOEXEC == 0 { return Err(KernelInventoryError::Process); }
        let namespace = current_net()?; let boot = BootFile::capture()?;
        let probe = socket(libc::AF_INET, libc::SOCK_DGRAM, 0)?;
        let net_cookie = cookie(probe.as_raw_fd())?;
        // Subscribe BEFORE any future dump. Keep default ENOBUFS reporting on.
        let monitor = RouteSocket::bind(ADDRESS_GROUPS, net_cookie)?;
        let owner = Arc::new(Self { process, pidfd, namespace, boot, probe, net_cookie, monitor,
            clock, state: Mutex::new(State { last: start, revision: 1, sequence: 0 }), fenced: AtomicBool::new(false) });
        owner.check(1, deadline)?;
        Ok(owner)
    }
    fn now(&self) -> Result<Sample> {
        Ok(Sample { boot_ns: self.clock.read().map_err(|_| KernelInventoryError::Clock)?.nanoseconds(), wall_ms: wall()? })
    }
    fn sample(&self, state: &mut State) -> Result<Sample> {
        let result = (|| {
            let at = self.now()?;
            if at.wall_ms < state.last.wall_ms || at.boot_ns < state.last.boot_ns { return Err(KernelInventoryError::Clock); }
            state.last = at; Ok(at)
        })(); self.finish(result)
    }
    fn custody(&self) -> Result<()> {
        let result = (|| {
            let mut poll = libc::pollfd { fd: self.pidfd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            if self.process != std::process::id() || unsafe { libc::poll(&mut poll, 1, 0) } != 0 {
                return Err(KernelInventoryError::Process);
            }
            if self.namespace.identity() != current_net()?.identity() || cookie(self.probe.as_raw_fd())? != self.net_cookie
                || cookie(self.monitor.fd.as_raw_fd())? != self.net_cookie { return Err(KernelInventoryError::Namespace); }
            self.boot.check()
        })(); self.finish(result)
    }
    fn monitor_current(&self, state: &mut State) -> Result<()> {
        let result = (|| {
            if let Some(packet) = receive(self.monitor.fd.as_raw_fd())? { self.observe_change(state, packet)?; }
            Ok(())
        })(); self.finish(result)
    }
    fn observe_change(&self, state: &mut State, packet: Packet) -> Result<()> {
        // Invalidate before releasing state or inspecting further event metadata.
        self.fenced.store(true, Ordering::Release);
        state.revision = state.revision.checked_add(1).ok_or(KernelInventoryError::Limit)?;
        if packet.sender != 0 { return Err(KernelInventoryError::Incomplete); }
        let frames = frames(&packet.bytes)?;
        if frames.iter().any(|frame|frame.kind == NLMSG_OVERRUN) { return Err(KernelInventoryError::Incomplete); }
        Err(KernelInventoryError::Changed) // Any link/address or unknown message freezes this owner.
    }
    fn finish<T>(&self, result: Result<T>) -> Result<T> {
        if result.as_ref().is_err_and(|error|*error != KernelInventoryError::Deadline) { self.fenced.store(true, Ordering::Release); }
        // The final acquire on every success is its absorbing-fence validation
        // linearization point. A concurrent fence may be set outside state's
        // mutex; a successful earlier custody/sample check cannot override it.
        // This observation does not lock out fencing or kernel mutation later.
        if result.is_ok() && self.fenced.load(Ordering::Acquire) { return Err(KernelInventoryError::Fenced); }
        result
    }
    fn check(&self, revision: u64, deadline: KernelSnapshotDeadline) -> Result<()> {
        let result = (|| {
            if self.fenced.load(Ordering::Acquire) { return Err(KernelInventoryError::Fenced); }
            deadline.check(self.now()?)?; // Includes time queued before this lock.
            let mut state = self.state.lock().map_err(|_| KernelInventoryError::Fenced)?;
            if self.fenced.load(Ordering::Acquire) || state.revision != revision { return Err(KernelInventoryError::Fenced); }
            deadline.check(self.sample(&mut state)?)?;
            self.custody()?; self.monitor_current(&mut state)?;
            deadline.check(self.sample(&mut state)?)
        })(); self.finish(result)
    }
    pub(crate) fn snapshot(self: &Arc<Self>, lifetime: Duration) -> Result<KernelIpv4Snapshot> {
        self.snapshot_inner(lifetime, || Ok(()))
    }
    fn snapshot_inner(self: &Arc<Self>, lifetime: Duration, after_dump: impl FnOnce() -> Result<()>) -> Result<KernelIpv4Snapshot> {
        let result = (|| {
            // Capture absolute bounds BEFORE queue/IO/dump work; never after callback return.
            let deadline = KernelSnapshotDeadline::begin(self.now()?, lifetime)?;
            let mut state = self.state.lock().map_err(|_| KernelInventoryError::Fenced)?;
            if self.fenced.load(Ordering::Acquire) { return Err(KernelInventoryError::Fenced); }
            deadline.check(self.sample(&mut state)?)?;
            self.custody()?; self.monitor_current(&mut state)?;
            state.sequence = state.sequence.checked_add(1).ok_or(KernelInventoryError::Limit)?;
            let addresses = self.finish(self.dump(&mut state, deadline))?;
            self.finish(after_dump())?; // Private late-work/fault seam; production is the no-op above.
            self.custody()?; self.monitor_current(&mut state)?;
            deadline.check(self.sample(&mut state)?)?;
            Ok(KernelIpv4Snapshot { owner: self.clone(), addresses, revision: state.revision, deadline })
        })(); self.finish(result)
    }
    /// Captured expectation DATA, not an authenticated Fly identity or fresh check.
    pub(crate) fn namespace_identity(&self) -> NamespaceIdentity { self.namespace.identity() }
    pub(crate) fn process_id(&self) -> u32 { self.process }
    pub(crate) fn netns_cookie(&self) -> u64 { self.net_cookie }
    pub(crate) fn boot_id(&self) -> &[u8] { &self.boot.bytes[..36] }
    fn dump(&self, state: &mut State, deadline: KernelSnapshotDeadline) -> Result<BTreeSet<Ipv4Addr>> {
        let socket = RouteSocket::bind(0, self.net_cookie)?;
        deadline.check(self.sample(state)?)?;
        socket.request_dump(state.sequence)?;
        let mut addresses = BTreeSet::new(); let mut bytes = 0usize; let mut messages = 0usize;
        loop {
            let at = self.sample(state)?; deadline.check(at)?;
            if let Some(packet) = receive(socket.fd.as_raw_fd())? {
                bytes = bytes.checked_add(packet.bytes.len()).ok_or(KernelInventoryError::Limit)?;
                if bytes > MAX_DUMP_BYTES { return Err(KernelInventoryError::Limit); }
                if consume_dump(&packet, state.sequence, &mut addresses, &mut messages)? { return Ok(addresses); }
            } else {
                let mut poll = libc::pollfd { fd: socket.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                let ready = unsafe { libc::poll(&mut poll, 1, deadline.poll_ms(at)?) };
                if ready < 0 || poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(KernelInventoryError::Incomplete);
                }
            }
        }
    }
}
impl KernelIpv4Snapshot {
    /// OFF-APP immediately before use. Queued changes/loss fence revision; there
    /// is no atomic lock against a subsequent kernel mutator. The future combined
    /// no-ingress owner must also enforce its independent immutable time bounds.
    pub(crate) fn check_current(&self) -> Result<()> { self.owner.check(self.revision, self.deadline) }
    pub(crate) fn addresses(&self) -> Result<&BTreeSet<Ipv4Addr>> { self.addresses_inner(|| Ok(())) }
    fn addresses_inner(&self, before_return: impl FnOnce() -> Result<()>) -> Result<&BTreeSet<Ipv4Addr>> {
        self.check_current()?;
        before_return()?; // Private concurrent-fence test seam; production is the no-op above.
        self.owner.finish(Ok(&self.addresses))
    }
    pub(crate) fn original_deadline(&self) -> KernelSnapshotDeadline { self.deadline }
    pub(crate) fn revision(&self) -> u64 { self.revision }
}
fn wall() -> Result<i64> {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)
        .map_err(|_| KernelInventoryError::Clock)?.as_millis()).map_err(|_| KernelInventoryError::Clock)
}

#[cfg(test)]
#[path = "kernel_address_inventory_tests.rs"]
mod tests;
