//! Fixed process slots, not timeout-wrapped getaddrinfo threads. A DNS slot is
//! never reused until its actual helper has replied or has been killed/reaped.
use super::{
    relay::{Runtime, Worker},
    socket::{set_nonblocking_cloexec, ProcessWitness},
    ConnectError as Error, ConnectErrorKind as Kind, ConnectResult as Result, ResolverProgram,
    MAX_RESOLVER_FRAME,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    io::{Read, Write},
    net::Ipv4Addr,
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{atomic::Ordering, mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    serial: u64,
    host: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    serial: u64,
    ok: bool,
    addresses: Vec<Ipv4Addr>,
}
struct Job {
    request: Request,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<Vec<Ipv4Addr>>>,
    _permit: DnsPermit,
}
pub(super) struct DnsPermit(Arc<Runtime>);
impl DnsPermit {
    pub(super) fn acquire(shared: &Arc<Runtime>) -> Result<Self> {
        let maximum = usize::from(shared.profile.budgets().max_dns_jobs);
        let mut active = shared.stats.active_dns.load(Ordering::Acquire);
        loop {
            if active >= maximum {
                return Err(Error::new(Kind::ResourceLimit));
            }
            match shared.stats.active_dns.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    shared.stats.max_dns.fetch_max(active + 1, Ordering::AcqRel);
                    return Ok(Self(Arc::clone(shared)));
                }
                Err(next) => active = next,
            }
        }
    }
}
impl Drop for DnsPermit {
    fn drop(&mut self) {
        self.0.stats.active_dns.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct ResolverPool {
    sender: mpsc::SyncSender<Job>,
}
impl ResolverPool {
    pub(crate) fn start(
        shared: &Arc<Runtime>,
        program: ResolverProgram,
    ) -> Result<(Self, Vec<Worker>)> {
        let count = usize::from(shared.profile.budgets().max_dns_jobs);
        let (sender, receiver) = mpsc::sync_channel::<Job>(count);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(count);
        for _ in 0..count {
            let shared = Arc::clone(shared);
            let receiver = Arc::clone(&receiver);
            let executable = Arc::clone(&program.executable);
            workers.push(Worker::spawn("rudder-connect-dns", move || {
                let Ok(mut helper) = Helper::spawn(&executable, &shared) else {
                    shared.fail(Kind::ResolverFailed);
                    return;
                };
                shared.resolver_ready.fetch_add(1, Ordering::AcqRel);
                loop {
                    if shared.cancelled() {
                        break;
                    }
                    let result = receiver
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_millis(20));
                    let job = match result {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    // Held until actual reply or actual process settlement, even
                    // when the requesting connection has already disappeared.
                    shared.stats.dns_jobs.fetch_add(1, Ordering::AcqRel);
                    let answer = helper.exchange(&job.request, job.deadline, &shared);
                    if answer.is_err() {
                        helper.kill_and_reap();
                        let _ = job.reply.try_send(answer);
                        shared.fail(Kind::ResolverFailed);
                        return;
                    }
                    let _ = job.reply.try_send(answer);
                }
                helper.kill_and_reap();
            })?);
        }
        Ok((Self { sender }, workers))
    }
    pub(crate) fn resolve(
        &self,
        host: &str,
        deadline: Instant,
        shared: &Arc<Runtime>,
    ) -> Result<Vec<Ipv4Addr>> {
        shared.check()?;
        if Instant::now() >= deadline {
            return Err(Error::new(Kind::Deadline));
        }
        let permit = DnsPermit::acquire(shared)?;
        let serial = shared.dns_serial.fetch_add(1, Ordering::AcqRel);
        let (reply, receiver) = mpsc::sync_channel(1);
        self.sender
            .try_send(Job {
                request: Request {
                    serial,
                    host: host.into(),
                },
                deadline,
                reply,
                _permit: permit,
            })
            .map_err(|_| Error::new(Kind::ResourceLimit))?;
        loop {
            shared.check()?;
            if Instant::now() >= deadline {
                return Err(Error::new(Kind::Deadline));
            }
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(answer) => return answer,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::new(Kind::ResolverFailed))
                }
            }
        }
    }
}

struct Helper {
    child: Child,
    process: ProcessWitness,
    input: ChildStdin,
    output: ChildStdout,
    reaped: bool,
}
impl Helper {
    fn spawn(executable: &std::fs::File, shared: &Runtime) -> Result<Self> {
        shared.host.check_current()?;
        let parent = unsafe { libc::getpid() };
        let mut command = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()));
        command
            .arg("--connected-network-resolver-v1")
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                    || libc::getppid() != parent
                    || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                let limit = libc::rlimit {
                    rlim_cur: 256 * 1024 * 1024,
                    rlim_max: 256 * 1024 * 1024,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|_| Error::new(Kind::ResolverFailed))?;
        let process = match ProcessWitness::capture(&mut child) {
            Ok(process) => process,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let input = child
            .stdin
            .take()
            .ok_or_else(|| Error::new(Kind::ResolverFailed))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| Error::new(Kind::ResolverFailed))?;
        let mut value = Self {
            child,
            process,
            input,
            output,
            reaped: false,
        };
        if set_nonblocking_cloexec(value.input.as_raw_fd()).is_err()
            || set_nonblocking_cloexec(value.output.as_raw_fd()).is_err()
            || shared.host.check_process(&value.process).is_err()
        {
            value.kill_and_reap();
            return Err(Error::new(Kind::ResolverFailed));
        }
        let deadline = Instant::now()
            + Duration::from_millis(u64::from(shared.profile.budgets().connect_timeout_ms));
        let mut ready = [0u8; 8];
        if transfer(
            value.output.as_raw_fd(),
            &mut ready,
            false,
            deadline,
            shared,
        )
        .is_err()
            || &ready != b"RDDRDN1\n"
        {
            value.kill_and_reap();
            return Err(Error::new(Kind::ResolverFailed));
        }
        Ok(value)
    }
    fn exchange(
        &mut self,
        request: &Request,
        deadline: Instant,
        shared: &Runtime,
    ) -> Result<Vec<Ipv4Addr>> {
        shared.check()?;
        self.process.verify_live()?;
        shared.host.check_process(&self.process)?;
        let bytes = serde_json::to_vec(request).map_err(|_| Error::new(Kind::ResolverFailed))?;
        if bytes.len() > MAX_RESOLVER_FRAME {
            return Err(Error::new(Kind::ResolverFailed));
        }
        let mut frame = Vec::with_capacity(bytes.len() + 4);
        frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        frame.extend_from_slice(&bytes);
        transfer(self.input.as_raw_fd(), &mut frame, true, deadline, shared)?;
        let mut length = [0u8; 4];
        transfer(
            self.output.as_raw_fd(),
            &mut length,
            false,
            deadline,
            shared,
        )?;
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_RESOLVER_FRAME {
            return Err(Error::new(Kind::ResolverFailed));
        }
        let mut bytes = vec![0u8; length];
        transfer(self.output.as_raw_fd(), &mut bytes, false, deadline, shared)?;
        let reply: Reply =
            serde_json::from_slice(&bytes).map_err(|_| Error::new(Kind::ResolverFailed))?;
        self.process.verify_live()?;
        shared.host.check_process(&self.process)?;
        if reply.serial != request.serial
            || !reply.ok
            || reply.addresses.is_empty()
            || reply.addresses.len() > super::super::MAX_ORIGIN_ADDRESSES
            || !reply.addresses.windows(2).all(|pair| pair[0] < pair[1])
        {
            return Err(Error::new(Kind::ResolverFailed));
        }
        Ok(reply.addresses)
    }
    fn kill_and_reap(&mut self) {
        if self.reaped {
            return;
        }
        self.process.terminate();
        // A D-state child keeps this fixed worker occupied. Outer close may
        // time out; it cannot claim settlement or start a replacement helper.
        while !self.process.exited() {
            thread::sleep(Duration::from_millis(10));
        }
        // A wait failure is not a reaping receipt. Retain this fixed worker and
        // its owner rather than allowing a replacement or successful close.
        while self.child.wait().is_err() {
            thread::sleep(Duration::from_millis(10));
        }
        self.reaped = true;
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

fn transfer(
    fd: i32,
    bytes: &mut [u8],
    write: bool,
    deadline: Instant,
    shared: &Runtime,
) -> Result<()> {
    let mut position = 0;
    while position < bytes.len() {
        shared.check_cancelled()?;
        if Instant::now() >= deadline {
            return Err(Error::new(Kind::Deadline));
        }
        let count = unsafe {
            if write {
                libc::write(
                    fd,
                    bytes[position..].as_ptr().cast(),
                    bytes.len() - position,
                )
            } else {
                libc::read(
                    fd,
                    bytes[position..].as_mut_ptr().cast(),
                    bytes.len() - position,
                )
            }
        };
        if count > 0 {
            position += count as usize;
            continue;
        }
        if count == 0 {
            return Err(Error::new(Kind::ResolverFailed));
        }
        let errno = std::io::Error::last_os_error().raw_os_error();
        if errno == Some(libc::EINTR) {
            continue;
        }
        if errno != Some(libc::EAGAIN) {
            return Err(Error::new(Kind::ResolverFailed));
        }
        let mut poll = libc::pollfd {
            fd,
            events: if write { libc::POLLOUT } else { libc::POLLIN },
            revents: 0,
        };
        unsafe {
            libc::poll(&mut poll, 1, 10);
        }
    }
    Ok(())
}

/// Dedicated fresh process entrypoint. Only bounded public host names enter,
/// and only bounded IPv4 answers leave. The host process never calls DNS here.
pub fn run_resolver_process() -> Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    output
        .write_all(b"RDDRDN1\n")
        .and_then(|_| output.flush())
        .map_err(|_| Error::new(Kind::Io))?;
    let mut requests = 0;
    loop {
        let mut length = [0u8; 4];
        match input.read_exact(&mut length) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(_) => return Err(Error::new(Kind::ResolverFailed)),
        }
        let length = u32::from_be_bytes(length) as usize;
        requests += 1;
        if requests > super::MAX_LIFETIME_CONNECTIONS {
            return Err(Error::new(Kind::ResourceLimit));
        }
        if length == 0 || length > MAX_RESOLVER_FRAME {
            return Err(Error::new(Kind::ResolverFailed));
        }
        let mut bytes = vec![0; length];
        input
            .read_exact(&mut bytes)
            .map_err(|_| Error::new(Kind::ResolverFailed))?;
        let request: Request =
            serde_json::from_slice(&bytes).map_err(|_| Error::new(Kind::ResolverFailed))?;
        if !super::super::canonical_public_hostname(&request.host) {
            return Err(Error::new(Kind::ResolverFailed));
        }
        let addresses = resolve_ipv4(&request.host);
        let reply = Reply {
            serial: request.serial,
            ok: addresses.is_ok(),
            addresses: addresses.unwrap_or_default(),
        };
        let bytes = serde_json::to_vec(&reply).map_err(|_| Error::new(Kind::ResolverFailed))?;
        if bytes.len() > MAX_RESOLVER_FRAME {
            return Err(Error::new(Kind::ResolverFailed));
        }
        output
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .and_then(|_| output.write_all(&bytes))
            .and_then(|_| output.flush())
            .map_err(|_| Error::new(Kind::ResolverFailed))?;
    }
}
fn resolve_ipv4(host: &str) -> Result<Vec<Ipv4Addr>> {
    // Absolute DNS name prevents resolver search-list expansion. No service
    // lookup and no connect-by-name path exists in the host transport.
    let host = CString::new(format!("{host}.")).map_err(|_| Error::new(Kind::ResolverFailed))?;
    let hints = libc::addrinfo {
        ai_flags: 0,
        ai_family: libc::AF_INET,
        ai_socktype: libc::SOCK_STREAM,
        ai_protocol: libc::IPPROTO_TCP,
        ai_addrlen: 0,
        ai_addr: std::ptr::null_mut(),
        ai_canonname: std::ptr::null_mut(),
        ai_next: std::ptr::null_mut(),
    };
    let mut first = std::ptr::null_mut();
    if unsafe { libc::getaddrinfo(host.as_ptr(), std::ptr::null(), &hints, &mut first) } != 0 {
        return Err(Error::new(Kind::ResolverFailed));
    }
    let mut addresses = std::collections::BTreeSet::new();
    let mut next = first;
    let mut count = 0;
    let result = loop {
        if next.is_null() {
            break Ok(addresses.into_iter().collect());
        }
        count += 1;
        if count > 256 {
            break Err(Error::new(Kind::ResolverFailed));
        }
        let entry = unsafe { &*next };
        if entry.ai_family != libc::AF_INET
            || entry.ai_addr.is_null()
            || entry.ai_addrlen as usize != std::mem::size_of::<libc::sockaddr_in>()
        {
            break Err(Error::new(Kind::ResolverFailed));
        }
        let address = unsafe { &*entry.ai_addr.cast::<libc::sockaddr_in>() };
        addresses.insert(Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()));
        if addresses.len() > super::super::MAX_ORIGIN_ADDRESSES {
            break Err(Error::new(Kind::ResolverFailed));
        }
        // Do not inspect/copy ai_canonname or arbitrary resolver text.
        next = entry.ai_next;
    };
    unsafe {
        libc::freeaddrinfo(first);
    }
    result.and_then(|addresses: Vec<_>| {
        if addresses.is_empty() {
            Err(Error::new(Kind::ResolverFailed))
        } else {
            Ok(addresses)
        }
    })
}
