//! Standalone, explicit no-key network qualification. No managed launch,
//! project/profile/account discovery, packet routes, provider auth or models.
use super::{
    socket::{self, ProcessWitness},
    *,
};
use crate::execution::network::{linux::*, *};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const MAX_PACKET: usize = 24 * 1024;
const CONTROL: RawFd = 3;
const CHILD_ARGUMENT: &str = "--connected-network-child-v1";
const PYTHON: &str = "/usr/bin/python3";
const TLS_CLIENT: &str = r#"
import json, os, socket, ssl, sys, urllib.parse
host, port = sys.argv[1], int(sys.argv[2])
proxy = urllib.parse.urlsplit(os.environ['HTTPS_PROXY'])
assert proxy.scheme == 'http' and proxy.hostname == '127.0.0.1' and proxy.port == port and not proxy.username and not proxy.password
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(8)
s.connect(('127.0.0.1', port))
s.sendall(('CONNECT '+host+':443 HTTP/1.1\r\nHost: '+host+':443\r\n\r\n').encode('ascii'))
header = b''
while not header.endswith(b'\r\n\r\n'):
    if len(header) >= 4096: raise RuntimeError('proxy header bound')
    part = s.recv(1)
    if not part: raise RuntimeError('proxy closed')
    header += part
assert header == b'HTTP/1.1 200 Connection Established\r\n\r\n'
context = ssl.create_default_context()
assert context.check_hostname and context.verify_mode == ssl.CERT_REQUIRED
with context.wrap_socket(s, server_hostname=host) as tls:
    tls.sendall(('HEAD / HTTP/1.1\r\nHost: '+host+'\r\nConnection: close\r\n\r\n').encode('ascii'))
    response = b''
    while b'\r\n\r\n' not in response:
        if len(response) >= 16384: raise RuntimeError('origin header bound')
        part = tls.recv(min(4096, 16384-len(response)))
        if not part: raise RuntimeError('origin closed before header')
        response += part
    line = response.split(b'\r\n',1)[0].split(b' ')
    assert line[0] in (b'HTTP/1.1', b'HTTP/1.0') and len(line[1]) == 3 and line[1].isdigit()
    status = int(line[1])
    assert 100 <= status <= 599
    print(json.dumps({'kind':'public-tls-head-v1','certificateVerified':True,'tlsVersion':tls.version(),'httpStatus':status,'responseHeaderBytes':len(response)}, separators=(',',':')))
"#;

struct Fixture {
    path: PathBuf,
    success: bool,
}
impl Fixture {
    fn create(base: &Path) -> ConnectResult<Self> {
        let metadata = fs::symlink_metadata(base)
            .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidConfiguration))?;
        if !base.is_absolute()
            || !metadata.is_dir()
            || metadata.uid() != 1500
            || metadata.mode() & 0o7777 != 0o700
            || fs::canonicalize(base).ok().as_deref() != Some(base)
        {
            return Err(ConnectError::new(ConnectErrorKind::InvalidConfiguration));
        }
        let path = base.join(format!("connect-{}", Uuid::new_v4()));
        fs::create_dir(&path).map_err(|_| ConnectError::new(ConnectErrorKind::Io))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .map_err(|_| ConnectError::new(ConnectErrorKind::Io))?;
        Ok(Self {
            path,
            success: false,
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.success {
            let _ = fs::remove_dir_all(&self.path);
        } else {
            let _ = private_failure(&self.path.join("failure.private.txt"),b"Qualification failed; see static outer error and any bounded child diagnostics.\n",b"");
            if let Some(name) = self.path.file_name().and_then(|name| name.to_str()) {
                eprintln!(
                    "{}",
                    json!({"kind":"connected-network-private-failure-location","fixture":name})
                );
            }
        }
    }
}
fn private_failure(path: &Path, errors: &[u8], output: &[u8]) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    file.write_all(b"Synthetic no-account child diagnostics; bounded private only.\nstderr:\n")?;
    file.write_all(&errors[..errors.len().min(7000)])?;
    file.write_all(b"\nstdout:\n")?;
    file.write_all(&output[..output.len().min(1000)])?;
    file.sync_all()
}

struct FixedAuthority {
    binding: ConnectBinding,
    active: AtomicBool,
    original_end: ConnectDeadline,
}
impl FixedAuthority {
    fn new(binding:ConnectBinding) -> Self {
        // Only the explicit synthetic qualifier. Fixed before any inventory/
        // validation callback; not a production authenticated inventory lease.
        Self { binding, active:AtomicBool::new(true), original_end:super::validity::qualifier_deadline(60_000) }
    }
}
impl ConnectValidationGuard for FixedAuthority {
    fn check_current(&self) -> ConnectResult<()> {
        if self.active.load(Ordering::Acquire) { Ok(()) }
        else { Err(ConnectError::new(ConnectErrorKind::AuthorityDenied)) }
    }
}
impl ConnectAuthority for FixedAuthority {
    fn validate(self:Arc<Self>, query:ConnectQuery) -> ConnectResult<ConnectValidation> {
        if query.binding() != &self.binding { return Err(ConnectError::new(ConnectErrorKind::AuthorityDenied)); }
        self.check_current()?;
        let inventory = trusted_interface_inventory()?;
        ConnectValidation::new(query, inventory, self.original_end, self.original_end, self.original_end, self)
    }
}
/// Trusted local interface inventory. In production this must also include
/// externally routed/NAT/public worker addresses from authenticated host setup.
/// The standalone tool claims only local-interface inventory, not NAT discovery.
fn trusted_interface_inventory() -> ConnectResult<HostIpv4DenySet> {
    let mut first = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut first) } != 0 {
        return Err(ConnectError::os(ConnectErrorKind::Io));
    }
    let mut cursor = first;
    let mut addresses = Vec::new();
    let mut entries = 0;
    let result = loop {
        if cursor.is_null() {
            break HostIpv4DenySet::from_trusted_host_inventory(addresses)
                .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidConfiguration));
        }
        entries += 1;
        if entries > 256 {
            break Err(ConnectError::new(ConnectErrorKind::ResourceLimit));
        }
        let entry = unsafe { &*cursor };
        if !entry.ifa_addr.is_null()
            && unsafe { (*entry.ifa_addr).sa_family } as i32 == libc::AF_INET
        {
            let address = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
            addresses.push(Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()));
        }
        cursor = entry.ifa_next;
    };
    unsafe {
        libc::freeifaddrs(first);
    }
    result
}
fn validate_profile(profile: NetworkProfileV1) -> ConnectResult<ValidatedNetworkProfile> {
    let binding = profile.binding.clone();
    let nonce = profile.readiness_nonce.clone();
    let expected = ExpectedNetworkBinding {
        project_id: &binding.project_id,
        owner: &binding.owner,
        owner_epoch: binding.owner_epoch.value(),
        run_id: &binding.run_id,
        run_epoch: binding.run_epoch.value(),
        request_id: &binding.request_id,
        execution_id: &binding.execution_id,
        operation_id: &binding.operation_id,
        view_id: &binding.view_id,
        network_id: &binding.network_id,
        readiness_nonce: &nonce,
    };
    let host = trusted_interface_inventory()?;
    profile
        .validate(&expected, Some(&host))
        .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidConfiguration))
}
fn make_profile(host: &str, addresses: Vec<Ipv4Addr>) -> ConnectResult<NetworkProfileV1> {
    let policy = PublicIpv4HttpsPolicy {
        format_version: 1,
        origins: vec![QualificationOrigin {
            host: host.into(),
            port: 443,
            ipv4_addresses: addresses,
        }],
    };
    let policy_sha256 = policy
        .sha256()
        .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidConfiguration))?;
    Ok(NetworkProfileV1 {
        format_version: 1,
        execution_profile: NetworkExecutionProfile::CloudUid1500LinuxV1,
        binding: NetworkBinding {
            project_id: "synthetic-connect".into(),
            owner: "synthetic-connect".into(),
            owner_epoch: NetworkEpoch::new(1).unwrap(),
            run_id: "synthetic-connect".into(),
            run_epoch: NetworkEpoch::new(1).unwrap(),
            request_id: Uuid::new_v4().to_string(),
            execution_id: Uuid::new_v4().to_string(),
            operation_id: Uuid::new_v4().to_string(),
            view_id: Uuid::new_v4().to_string(),
            network_id: Uuid::new_v4().to_string(),
        },
        mode: NetworkMode::QualificationHttpsConnectV1,
        policy: Some(policy),
        policy_sha256: Some(policy_sha256),
        readiness_nonce: Uuid::new_v4().to_string(),
        budgets: NetworkBudgets {
            max_connections: 2,
            max_dns_jobs: 1,
            max_request_bytes: 4096,
            max_buffer_bytes: 32768,
            max_bytes_per_connection: 1024 * 1024,
            max_total_bytes: 2 * 1024 * 1024,
            connect_timeout_ms: 8000,
            idle_timeout_ms: 10000,
            total_timeout_ms: 20000,
        },
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildConfig {
    profile: NetworkProfileV1,
    private_home: PathBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildFailure {
    kind: String,
    error: ConnectErrorKind,
    errno: Option<i32>,
}
fn receive_child(fd: RawFd, deadline: Instant) -> ConnectResult<(Vec<u8>, Vec<OwnedFd>)> {
    let result = receive_packet(fd, deadline)?;
    if let Ok(failure) = serde_json::from_slice::<ChildFailure>(&result.0) {
        if failure.kind == "connected-child-failure-v1" && result.1.is_empty() {
            return Err(ConnectError {
                kind: failure.error,
                errno: failure.errno,
            });
        }
    }
    Ok(result)
}

/// Only this owned fresh helper creates a new USER/NET. The supervisor never
/// calls setns and never moves a host thread into the guest network namespace.
fn child_main() -> ConnectResult<()> {
    unsafe {
        libc::alarm(35);
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(ConnectError::os(ConnectErrorKind::InvalidProcess));
    }
    let control = unsafe { OwnedFd::from_raw_fd(CONTROL) };
    socket::set_nonblocking_cloexec(control.as_raw_fd())?;
    let result = child_inner(&control);
    if let Err(error) = result {
        let failure = ChildFailure {
            kind: "connected-child-failure-v1".into(),
            error: error.kind(),
            errno: error.raw_os_error(),
        };
        if let Ok(bytes) = serde_json::to_vec(&failure) {
            let _ = send_packet(
                control.as_raw_fd(),
                &bytes,
                &[],
                Instant::now() + Duration::from_millis(100),
            );
        }
    }
    result
}
fn child_inner(control: &OwnedFd) -> ConnectResult<()> {
    let deadline = Instant::now() + Duration::from_secs(25);
    let (bytes, fds) = receive_packet(control.as_raw_fd(), deadline)?;
    if !fds.is_empty() {
        return Err(ConnectError::new(ConnectErrorKind::InvalidConfiguration));
    }
    let config: ChildConfig = serde_json::from_slice(&bytes)
        .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidConfiguration))?;
    let profile = validate_profile(config.profile)?;
    let host = profile.policy().unwrap().origins[0].host.clone();
    let baseline = ChildNetworkBaseline::capture(&profile).map_err(network_error)?;
    let network =
        unsafe { baseline.enter_new_user_and_net_for_qualification() }.map_err(network_error)?;
    network.verify_current().map_err(network_error)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|_| ConnectError::os(ConnectErrorKind::Io))?;
    listener
        .set_nonblocking(true)
        .map_err(|_| ConnectError::new(ConnectErrorKind::Io))?;
    let frame = ListenerFrame::for_listener(&profile, &listener)?;
    let port = frame.port;
    let namespaces = network.child_namespace_fds();
    send_packet(
        control.as_raw_fd(),
        &serde_json::to_vec(&frame)
            .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidConfiguration))?,
        &[namespaces[0], namespaces[1], listener.as_fd()],
        deadline,
    )?;
    let listener_fd = listener.as_raw_fd();
    drop(listener);
    if unsafe { libc::fcntl(listener_fd, libc::F_GETFD) } != -1
        || std::io::Error::last_os_error().raw_os_error() != Some(libc::EBADF)
    {
        return Err(ConnectError::new(ConnectErrorKind::InvalidListener));
    }
    let (ack, fds) = receive_packet(control.as_raw_fd(), deadline)?;
    if ack != b"ADMIT1" || !fds.is_empty() {
        return Err(ConnectError::new(ConnectErrorKind::BindingMismatch));
    }
    network.verify_current().map_err(network_error)?;
    let proof = unsafe { drop_all_capabilities_for_exec() }.map_err(network_error)?;
    proof.verify_current_for_exec().map_err(network_error)?;
    // Remove every namespace/control authority FD before untrusted exec. The
    // parent independently checks actual /proc caps while this helper waits.
    drop(network);
    send_packet(control.as_raw_fd(), b"ZERO5-NNP1", &[], deadline)?;
    let (ack, fds) = receive_packet(control.as_raw_fd(), deadline)?;
    if ack != b"EXEC1" || !fds.is_empty() {
        return Err(ConnectError::new(ConnectErrorKind::BindingMismatch));
    }
    proof.verify_current_for_exec().map_err(network_error)?;
    // This borrowed control descriptor is CLOEXEC and must not enter Python.
    let error = Command::new(PYTHON)
        .arg("-I")
        .arg("-c")
        .arg(TLS_CLIENT)
        .arg(host)
        .arg(port.to_string())
        .env_clear()
        .env("HOME", &config.private_home)
        .env("HTTPS_PROXY", format!("http://127.0.0.1:{port}"))
        .env("NO_PROXY", "")
        .current_dir(&config.private_home)
        .exec();
    let _ = error;
    Err(ConnectError::new(ConnectErrorKind::Io))
}

fn network_error(error: NetworkError) -> ConnectError {
    ConnectError {
        kind: ConnectErrorKind::InvalidNamespace,
        errno: error.raw_os_error(),
    }
}
fn channel() -> ConnectResult<[OwnedFd; 2]> {
    let mut fds = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(ConnectError::os(ConnectErrorKind::Io));
    }
    Ok(unsafe { [OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])] })
}
fn send_packet(
    fd: RawFd,
    bytes: &[u8],
    fds: &[BorrowedFd<'_>],
    deadline: Instant,
) -> ConnectResult<()> {
    if bytes.is_empty() || bytes.len() > MAX_PACKET || fds.len() > 3 {
        return Err(ConnectError::new(ConnectErrorKind::InvalidConfiguration));
    }
    let mut vector = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
    let mut control = vec![0usize; space.div_ceil(std::mem::size_of::<usize>())];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    if !fds.is_empty() {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len =
                libc::CMSG_LEN((fds.len() * std::mem::size_of::<RawFd>()) as u32) as usize;
            let target = libc::CMSG_DATA(header).cast::<RawFd>();
            for (index, value) in fds.iter().enumerate() {
                target.add(index).write(value.as_raw_fd());
            }
        }
    }
    loop {
        if Instant::now() >= deadline {
            return Err(ConnectError::new(ConnectErrorKind::Deadline));
        }
        let count = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
        if count == bytes.len() as isize {
            return Ok(());
        }
        if count >= 0 {
            return Err(ConnectError::new(ConnectErrorKind::Io));
        }
        if !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN | libc::EINTR)
        ) {
            return Err(ConnectError::os(ConnectErrorKind::Io));
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn receive_packet(fd: RawFd, deadline: Instant) -> ConnectResult<(Vec<u8>, Vec<OwnedFd>)> {
    let mut bytes = vec![0u8; MAX_PACKET];
    let space = unsafe { libc::CMSG_SPACE((3 * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0usize; space.div_ceil(std::mem::size_of::<usize>())];
    loop {
        if Instant::now() >= deadline {
            return Err(ConnectError::new(ConnectErrorKind::Deadline));
        }
        let mut vector = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space;
        let count = unsafe {
            libc::recvmsg(
                fd,
                &mut message,
                libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT,
            )
        };
        if count < 0 {
            if !matches!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EAGAIN | libc::EINTR)
            ) {
                return Err(ConnectError::os(ConnectErrorKind::Io));
            }
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        let mut owned = Vec::new();
        let mut invalid = count == 0;
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&message);
            while !header.is_null() {
                let header_size = libc::CMSG_LEN(0) as usize;
                if (*header).cmsg_level != libc::SOL_SOCKET
                    || (*header).cmsg_type != libc::SCM_RIGHTS
                    || (*header).cmsg_len < header_size
                {
                    invalid = true;
                    break;
                }
                let length = (*header).cmsg_len - header_size;
                if length % std::mem::size_of::<RawFd>() != 0 {
                    invalid = true;
                    break;
                }
                for index in 0..length / std::mem::size_of::<RawFd>() {
                    owned.push(OwnedFd::from_raw_fd(
                        libc::CMSG_DATA(header).cast::<RawFd>().add(index).read(),
                    ));
                }
                header = libc::CMSG_NXTHDR(&message, header);
            }
        }
        if invalid
            || message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
            || owned.len() > 3
        {
            return Err(ConnectError::new(ConnectErrorKind::InvalidConfiguration));
        }
        bytes.truncate(count as usize);
        return Ok((bytes, owned));
    }
}

struct OwnedChild {
    child: Child,
    witness: ProcessWitness,
    reaped: bool,
    diagnostics: Option<PathBuf>,
}
impl OwnedChild {
    fn spawn(control: &OwnedFd) -> ConnectResult<Self> {
        let source = control.as_raw_fd();
        let parent = unsafe { libc::getpid() };
        let program = ResolverProgram::current_executable()?;
        let mut command = program.command();
        command
            .arg(CHILD_ARGUMENT)
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                    || libc::getppid() != parent
                    || (source != CONTROL && libc::dup2(source, CONTROL) < 0)
                    || libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) != 0
                    || libc::fcntl(CONTROL, libc::F_SETFD, 0) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|_| ConnectError::new(ConnectErrorKind::Io))?;
        let witness = match ProcessWitness::capture(&mut child) {
            Ok(value) => value,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            witness,
            reaped: false,
            diagnostics: None,
        })
    }
    fn wait_output(&mut self, deadline: Instant, expected_success: bool) -> ConnectResult<Vec<u8>> {
        let mut stdout = self
            .child
            .stdout
            .take()
            .ok_or_else(|| ConnectError::new(ConnectErrorKind::InvalidProcess))?;
        let mut stderr = self
            .child
            .stderr
            .take()
            .ok_or_else(|| ConnectError::new(ConnectErrorKind::InvalidProcess))?;
        socket::set_nonblocking_cloexec(stdout.as_raw_fd())?;
        socket::set_nonblocking_cloexec(stderr.as_raw_fd())?;
        let mut output = Vec::with_capacity(4096);
        let mut errors = Vec::with_capacity(4096);
        let mut observed_exit = false;
        loop {
            let mut buffer = [0u8; 2048];
            for (pipe, keep) in [
                (&mut stdout as &mut dyn Read, true),
                (&mut stderr as &mut dyn Read, false),
            ] {
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            if keep {
                                output.extend_from_slice(&buffer[..count]);
                            } else {
                                errors.extend_from_slice(&buffer[..count]);
                            }
                            if output.len() > 8192 || errors.len() > 8192 {
                                if let Some(path) = &self.diagnostics {
                                    let _ = private_failure(path, &errors, &output);
                                }
                                return Err(ConnectError::new(ConnectErrorKind::ResourceLimit));
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => return Err(ConnectError::new(ConnectErrorKind::Io)),
                    }
                }
            }
            if self.witness.exited() {
                if !observed_exit {
                    observed_exit = true;
                    continue;
                }
                let status = self
                    .child
                    .wait()
                    .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidProcess))?;
                self.reaped = true;
                if status.success() != expected_success || !errors.is_empty() {
                    if let Some(path) = &self.diagnostics {
                        let _ = private_failure(path, &errors, &output);
                    }
                    return Err(ConnectError::new(ConnectErrorKind::ConnectFailed));
                }
                return Ok(output);
            }
            if Instant::now() >= deadline {
                return Err(ConnectError::new(ConnectErrorKind::Deadline));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            self.witness.terminate();
            // Retain ownership until actual settlement. This standalone tool
            // has a global alarm; it never fabricates a reaped-child report.
            while !self.witness.exited() {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}
fn zero5(pid: u32) -> ConnectResult<()> {
    let bytes = socket::read_proc(&format!("/proc/{pid}/status"), 65536)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidProcess))?;
    for field in ["CapEff:", "CapPrm:", "CapInh:", "CapAmb:", "CapBnd:"] {
        if text
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .map(str::trim)
            != Some("0000000000000000")
        {
            return Err(ConnectError::new(ConnectErrorKind::InvalidProcess));
        }
    }
    if text
        .lines()
        .find_map(|line| line.strip_prefix("NoNewPrivs:"))
        .map(str::trim)
        != Some("1")
    {
        return Err(ConnectError::new(ConnectErrorKind::InvalidProcess));
    }
    Ok(())
}
fn loopback_only(pid: u32) -> ConnectResult<()> {
    let bytes = socket::read_proc(&format!("/proc/{pid}/net/dev"), 65536)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidNamespace))?;
    let interfaces: Vec<_> = text
        .lines()
        .skip(2)
        .map(|line| line.split_once(':').map(|(name, _)| name.trim()))
        .collect();
    if interfaces != [Some("lo")] {
        return Err(ConnectError::new(ConnectErrorKind::InvalidNamespace));
    }
    Ok(())
}
fn qualify(base: &Path, host: &str, addresses: Vec<Ipv4Addr>) -> ConnectResult<Value> {
    let mut fixture = Fixture::create(base)?;
    let raw_profile = make_profile(host, addresses)?;
    let profile = validate_profile(raw_profile.clone())?;
    let digest = hex::encode(profile.profile_digest());
    let binding = ConnectBinding::for_profile(&profile)?;
    let authority = Arc::new(FixedAuthority::new(binding));
    let host_anchor = HostNetworkAnchor::capture()?;
    let [control, remote] = channel()?;
    let mut child = OwnedChild::spawn(&remote)?;
    child.diagnostics = Some(fixture.path.join("failure.private.txt"));
    drop(remote);
    let deadline = Instant::now() + Duration::from_secs(25);
    let config = ChildConfig {
        profile: raw_profile,
        private_home: fixture.path.clone(),
    };
    send_packet(
        control.as_raw_fd(),
        &serde_json::to_vec(&config)
            .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidConfiguration))?,
        &[],
        deadline,
    )?;
    let (bytes, descriptors) = receive_child(control.as_raw_fd(), deadline)?;
    let frame: ListenerFrame = serde_json::from_slice(&bytes)
        .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidListener))?;
    let admission = ListenerAdmission::receive(&profile, &mut child.child, frame, descriptors)?;
    let net = admission.witnesses.network.identity();
    let user = NamespaceFd::for_process(child.child.id(), NamespaceKind::User)
        .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidNamespace))?
        .identity();
    let port = admission.port();
    let mut retained_proxy = None;
    let result = (|| {
        OwnedConnectProxy::start_retaining(
            profile, admission, host_anchor, authority.clone(),
            ResolverProgram::current_executable()?, &mut retained_proxy,
        )?;
        let proxy = retained_proxy.as_mut()
            .ok_or_else(|| ConnectError::new(ConnectErrorKind::Unsettled))?;
        proxy.await_ready(Duration::from_secs(10))?;
        send_packet(control.as_raw_fd(), b"ADMIT1", &[], deadline)?;
        let (bytes, descriptors) = receive_child(control.as_raw_fd(), deadline)?;
        if bytes != b"ZERO5-NNP1" || !descriptors.is_empty() {
            return Err(ConnectError::new(ConnectErrorKind::InvalidProcess));
        }
        child.witness.verify_live()?;
        zero5(child.child.id())?;
        loopback_only(child.child.id())?;
        if NamespaceFd::for_process(child.child.id(), NamespaceKind::Network)
            .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidNamespace))?
            .identity()
            != net
            || NamespaceFd::for_process(child.child.id(), NamespaceKind::User)
                .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidNamespace))?
                .identity()
                != user
        {
            return Err(ConnectError::new(ConnectErrorKind::InvalidNamespace));
        }
        child.witness.verify_live()?;
        send_packet(control.as_raw_fd(), b"EXEC1", &[], deadline)?;
        let output = child.wait_output(deadline, true)?;
        let result: Value = serde_json::from_slice(&output)
            .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidProcess))?;
        if result["kind"] != "public-tls-head-v1"
            || result["certificateVerified"] != true
            || result["responseHeaderBytes"]
                .as_u64()
                .is_none_or(|size| size > 16384)
        {
            return Err(ConnectError::new(ConnectErrorKind::InvalidProcess));
        }
        Ok(result)
    })();
    // Include partial startup and failed readiness in the settlement attempt.
    // This standalone CLI never reports qualification on unsettled ownership.
    let settlement = retained_proxy.as_mut()
        .map(|proxy| proxy.close_and_settle(Duration::from_secs(8))).transpose();
    let client = result?;
    let settlement = settlement?
        .ok_or_else(|| ConnectError::new(ConnectErrorKind::Unsettled))?;
    let counters = settlement.snapshot();
    if counters.upstream_connections != 1
        || counters.dns_jobs != 1
        || counters.active_connections != 0
        || counters.active_dns_jobs != 0
        || counters.charged_bytes == 0
    {
        return Err(ConnectError::new(ConnectErrorKind::Unsettled));
    }
    fixture.success = true;
    Ok(
        json!({"kind":"connected-network-qualification-v1","status":"passed","transportVersion":1,
        "networkProfileVersion":1,"profileSha256":digest,"listenerPort":port,
        "physical":{"uid":1500,"separateUser":true,"separateNetwork":true,"socketNamespaceIoctl":true,
            "listenerRemovedBeforeExec":true,"allFiveCapabilitiesZero":true,"noNewPrivileges":true,"loOnly":true,
            "actualGuestReaped":child.reaped,"resolverChildrenReaped":true,"proxyWorkersJoined":true},
        "guestUser":{"device":user.device().to_string(),"inode":user.inode().to_string()},
        "guestNetwork":{"device":net.device().to_string(),"inode":net.inode().to_string()},
        "client":client,"counters":counters,
        "limits":{"lifetimeConnections":MAX_LIFETIME_CONNECTIONS,"dnsProcesses":1,"connectionWorkers":2,
            "combinedBufferBytesPerConnection":32768,"totalBytes":2097152,"connectionTotalMs":20000},
        "limitations":["standalone synthetic current grant; no managed connected execution activation",
            "public no-key Python TLS HEAD; no Claude/Codex/model/account calls",
            "local interface inventory only; production needs authenticated public/NAT host inventory",
            "IPv4 HTTPS CONNECT only; tunnel contents and inner TLS authority are not inspected",
            "no private root, PID namespace, PTY, filesystem, VCS or checkpoint claim"]}),
    )
}

fn fresh_uid1500() -> ConnectResult<()> {
    if unsafe { libc::getuid() } == 0 {
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0
            || unsafe { libc::setresgid(1500, 1500, 1500) } != 0
            || unsafe { libc::setresuid(1500, 1500, 1500) } != 0
        {
            return Err(ConnectError::os(ConnectErrorKind::InvalidProcess));
        }
        let error = Command::new(
            std::env::current_exe().map_err(|_| ConnectError::new(ConnectErrorKind::Io))?,
        )
        .args(std::env::args().skip(1))
        .env_clear()
        .current_dir("/")
        .exec();
        let _ = error;
        return Err(ConnectError::new(ConnectErrorKind::Io));
    }
    if unsafe { libc::getuid() } != 1500
        || unsafe { libc::getgid() } != 1500
        || unsafe { libc::getgroups(0, std::ptr::null_mut()) } != 0
        || unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
    {
        return Err(ConnectError::new(ConnectErrorKind::InvalidProcess));
    }
    Ok(())
}
/// No network runs by default. Explicit qualify requires an owned private base,
/// canonical public host and exact comma-separated sorted IPv4 policy pins.
pub fn run_cli(arguments: &[String]) -> i32 {
    if arguments == ["--describe"] {
        println!(
            "{}",
            json!({"kind":"connected-network-qualifier","transportVersion":1,"defaultNetworkEffects":false,
            "usage":"qualify --base ABS_PRIVATE_UID1500_DIR --host PUBLIC_FQDN --ipv4 SORTED_COMMA_SEPARATED_PINS",
            "managedConnectedActivation":false,"futureManagedProtocolAllocation":"proposal Spec5/Binding4; not allocated here"})
        );
        return 0;
    }
    let mode = arguments.first().map(String::as_str);
    if mode == Some("--connected-network-resolver-v1") {
        return if run_resolver_process().is_ok() { 0 } else { 1 };
    }
    if mode == Some(CHILD_ARGUMENT) {
        return match child_main() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                1
            }
        };
    }
    if mode == Some("--local-ipc-selftest") {
        unsafe {
            libc::alarm(10);
        }
        return match local_ipc_selftest() {
            Ok(()) => {
                println!(
                    "{}",
                    json!({"kind":"connected-network-local-ipc","status":"passed","networkEffects":false})
                );
                0
            }
            Err(error) => {
                eprintln!("{error}");
                1
            }
        };
    }
    if mode == Some("--local-process-selftest") {
        unsafe {
            libc::alarm(10);
        }
        return match local_process_selftest() {
            Ok(()) => {
                println!(
                    "{}",
                    json!({"kind":"connected-network-local-process","status":"passed","pidfdKilled":true,"actualChildReaped":true,"networkEffects":false})
                );
                0
            }
            Err(error) => {
                eprintln!("{error}");
                1
            }
        };
    }
    if mode == Some("--local-resolver-frame-selftest") {
        unsafe {
            libc::alarm(10);
        }
        return match local_resolver_frame_selftest() {
            Ok(()) => {
                println!(
                    "{}",
                    json!({"kind":"connected-network-resolver-frame","status":"passed","oversizedFrameRejected":true,"actualChildReaped":true,"dnsQueries":0,"networkEffects":false})
                );
                0
            }
            Err(error) => {
                eprintln!("{error}");
                1
            }
        };
    }
    if mode == Some("--connected-network-held-child-selftest") {
        thread::sleep(Duration::from_secs(30));
        return 0;
    }
    if arguments.len() != 7
        || mode != Some("qualify")
        || arguments[1] != "--base"
        || arguments[3] != "--host"
        || arguments[5] != "--ipv4"
    {
        eprintln!("connected-network qualifier requires explicit qualify arguments or --describe");
        return 2;
    }
    unsafe {
        libc::alarm(45);
    }
    let result = (|| {
        fresh_uid1500()?;
        let addresses: Vec<Ipv4Addr> = arguments[6]
            .split(',')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidConfiguration))?;
        qualify(Path::new(&arguments[2]), &arguments[4], addresses)
    })();
    match result {
        Ok(proof) => {
            println!("{proof}");
            0
        }
        Err(error) => {
            let unsupported = matches!(
                error.kind(),
                ConnectErrorKind::SocketNamespaceUnavailable | ConnectErrorKind::InvalidNamespace
            ) && matches!(
                error.raw_os_error(),
                Some(libc::EPERM | libc::ENOSYS | libc::EINVAL)
            );
            println!(
                "{}",
                json!({"kind":"connected-network-qualification-v1","status":if unsupported {"unsupported"} else {"failed"},
                "error":error.kind(),"errno":error.raw_os_error(),"hostFallback":false,"managedConnectedActivation":false})
            );
            if unsupported {
                77
            } else {
                1
            }
        }
    }
}
fn local_ipc_selftest() -> ConnectResult<()> {
    let [sender, receiver] = channel()?;
    let file: OwnedFd = fs::File::open("/dev/null")
        .map_err(|_| ConnectError::new(ConnectErrorKind::Io))?
        .into();
    let deadline = Instant::now() + Duration::from_secs(1);
    send_packet(sender.as_raw_fd(), b"test", &[file.as_fd()], deadline)?;
    let (bytes, fds) = receive_packet(receiver.as_raw_fd(), deadline)?;
    if bytes != b"test"
        || fds.len() != 1
        || unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC == 0
    {
        return Err(ConnectError::new(ConnectErrorKind::InvalidConfiguration));
    }
    Ok(())
}
fn local_process_selftest() -> ConnectResult<()> {
    let host = HostNetworkAnchor::capture()?;
    let mut command =
        Command::new(std::env::current_exe().map_err(|_| ConnectError::new(ConnectErrorKind::Io))?);
    command
        .arg("--connected-network-held-child-selftest")
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    private_child(&mut command);
    let mut child = command
        .spawn()
        .map_err(|_| ConnectError::new(ConnectErrorKind::Io))?;
    let process = ProcessWitness::capture(&mut child)?;
    let mut owned = OwnedChild {
        child,
        witness: process,
        reaped: false,
        diagnostics: None,
    };
    host.check_process(&owned.witness)?;
    owned.witness.terminate();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !owned.witness.exited() {
        if Instant::now() >= deadline {
            return Err(ConnectError::new(ConnectErrorKind::Unsettled));
        }
        thread::sleep(Duration::from_millis(5));
    }
    let status = owned
        .child
        .wait()
        .map_err(|_| ConnectError::new(ConnectErrorKind::Unsettled))?;
    owned.reaped = true;
    use std::os::unix::process::ExitStatusExt;
    if status.signal() != Some(libc::SIGKILL) || owned.witness.verify_live().is_ok() {
        return Err(ConnectError::new(ConnectErrorKind::InvalidProcess));
    }
    Ok(())
}
fn local_resolver_frame_selftest() -> ConnectResult<()> {
    let host = HostNetworkAnchor::capture()?;
    let program = ResolverProgram::current_executable()?;
    let mut command = program.command();
    command
        .arg("--connected-network-resolver-v1")
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    private_child(&mut command);
    let mut child = command
        .spawn()
        .map_err(|_| ConnectError::new(ConnectErrorKind::Io))?;
    let witness = ProcessWitness::capture(&mut child)?;
    let mut owned = OwnedChild {
        child,
        witness,
        reaped: false,
        diagnostics: None,
    };
    host.check_process(&owned.witness)?;
    let mut input = owned
        .child
        .stdin
        .take()
        .ok_or_else(|| ConnectError::new(ConnectErrorKind::InvalidProcess))?;
    // The actual resolver rejects the length BEFORE allocating or reading the
    // body, before parsing a host and before any getaddrinfo call.
    input
        .write_all(&u32::MAX.to_be_bytes())
        .map_err(|_| ConnectError::new(ConnectErrorKind::Io))?;
    drop(input);
    let output = owned.wait_output(Instant::now() + Duration::from_secs(2), false)?;
    if output != b"RDDRDN1\n" || !owned.reaped {
        return Err(ConnectError::new(ConnectErrorKind::ResolverFailed));
    }
    Ok(())
}

fn private_child(command: &mut Command) {
    let parent = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                || libc::getppid() != parent
                || libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
pub(super) fn test_profile() -> ValidatedNetworkProfile {
    validate_profile(make_profile("example.com", vec![Ipv4Addr::new(93, 184, 216, 34)]).unwrap())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scm_rights_exact_owned_descriptors_and_cloexec() {
        local_ipc_selftest().unwrap();
    }
    #[test]
    fn synthetic_authority_is_exact_and_revocation_is_live() {
        let profile = validate_profile(
            make_profile("example.com", vec![Ipv4Addr::new(93, 184, 216, 34)]).unwrap(),
        )
        .unwrap();
        let binding = ConnectBinding::for_profile(&profile).unwrap();
        let authority = Arc::new(FixedAuthority::new(binding.clone()));
        let validation = authority.clone().validate(ConnectQuery::begin(&binding).unwrap()).unwrap();
        let mut wrong = binding.clone();
        wrong.network.owner = "other".into();
        assert!(authority.clone().validate(ConnectQuery::begin(&wrong).unwrap()).is_err());
        authority.active.store(false, Ordering::Release);
        assert!(validation.check_current().is_err());
        assert!(authority.clone().validate(ConnectQuery::begin(&binding).unwrap()).is_err());
    }
    #[test]
    fn private_failure_is_bounded_private_and_never_opens_an_existing_fifo_or_symlink() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("failure.private.txt");
        private_failure(&path, &vec![b'e'; 20000], &vec![b'o'; 20000]).unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file() && metadata.len() <= 8192 && metadata.mode() & 0o7777 == 0o600);
        let protected = root.path().join("protected");
        fs::write(&protected, b"unchanged").unwrap();
        let alias = root.path().join("alias");
        symlink(&protected, &alias).unwrap();
        assert!(private_failure(&alias, b"replace", b"").is_err());
        assert_eq!(fs::read(&protected).unwrap(), b"unchanged");
        let fifo = root.path().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert!(private_failure(&fifo, b"blocked", b"").is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
