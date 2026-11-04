use super::{
    ConnectBinding, ConnectError as Error, ConnectErrorKind as Kind, ConnectResult as Result,
};
use crate::execution::network::{
    linux::{verify_namespace_owner, NamespaceFd, NamespaceKind},
    ValidatedNetworkProfile,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    process::Child,
    sync::Arc,
};

const SIOCGSKNS: libc::c_ulong = 0x894c;
const SO_NETNS_COOKIE: i32 = 71;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerFrame {
    pub binding: ConnectBinding,
    pub readiness_nonce: String,
    pub port: u16,
    pub socket_cookie: u64,
}
impl ListenerFrame {
    /// Child-side frame data does not authenticate its sender. Admission below
    /// independently checks the actual Child, pidfd, namespaces and socket.
    pub fn for_listener(profile: &ValidatedNetworkProfile, listener: &TcpListener) -> Result<Self> {
        let SocketAddr::V4(address) = listener
            .local_addr()
            .map_err(|_| Error::new(Kind::InvalidListener))?
        else {
            return Err(Error::new(Kind::InvalidListener));
        };
        if *address.ip() != Ipv4Addr::LOCALHOST || address.port() == 0 {
            return Err(Error::new(Kind::InvalidListener));
        }
        Ok(Self {
            binding: ConnectBinding::for_profile(profile)?,
            readiness_nonce: profile.readiness_nonce().into(),
            port: address.port(),
            socket_cookie: cookie(listener.as_raw_fd())?,
        })
    }
}

/// A held pidfd for an actual unreaped std::process::Child. Never constructed
/// from a serialized PID, peer-credential claim, or `/proc` path supplied by a guest.
pub(crate) struct ProcessWitness {
    fd: OwnedFd,
    pid: u32,
}
impl ProcessWitness {
    pub(crate) fn capture(child: &mut Child) -> Result<Self> {
        if child
            .try_wait()
            .map_err(|_| Error::new(Kind::InvalidProcess))?
            .is_some()
        {
            return Err(Error::new(Kind::InvalidProcess));
        }
        let pid = child.id();
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        if fd < 0 {
            return Err(Error::os(Kind::InvalidProcess));
        }
        let value = Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            pid,
        };
        value.verify_live()?;
        Ok(value)
    }
    pub(crate) fn verify_live(&self) -> Result<()> {
        let mut poll = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, 0) };
        if result != 0 {
            return Err(Error::new(Kind::InvalidProcess));
        }
        let bytes = read_proc(&format!("/proc/self/fdinfo/{}", self.fd.as_raw_fd()), 4096)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| Error::new(Kind::InvalidProcess))?;
        let pid = text
            .lines()
            .find_map(|line| line.strip_prefix("Pid:\t"))
            .and_then(|value| value.parse::<u32>().ok());
        if pid != Some(self.pid) {
            return Err(Error::new(Kind::InvalidProcess));
        }
        Ok(())
    }
    pub(crate) fn check_namespace(
        &self,
        namespace: &NamespaceFd,
        kind: NamespaceKind,
    ) -> Result<()> {
        self.verify_live()?;
        let actual = NamespaceFd::for_process(self.pid, kind)
            .map_err(|_| Error::new(Kind::InvalidNamespace))?;
        if actual.identity() != namespace.identity() {
            return Err(Error::new(Kind::InvalidNamespace));
        }
        self.verify_live()
    }
    pub(crate) fn terminate(&self) {
        // The owned pidfd, never a recycled numeric PID or process-group ID.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd.as_raw_fd(),
                libc::SIGKILL,
                0usize,
                0,
            );
        }
    }
    pub(crate) fn exited(&self) -> bool {
        let mut fd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        (unsafe { libc::poll(&mut fd, 1, 0) }) == 1 && fd.revents & libc::POLLIN != 0
    }
}

pub(crate) fn read_proc(path: &str, maximum: usize) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|_| Error::new(Kind::InvalidProcess))?;
    let mut bytes = Vec::with_capacity(maximum.min(4096));
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::new(Kind::InvalidProcess))?;
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(Error::new(Kind::InvalidProcess));
    }
    Ok(bytes)
}
fn cloud_identity(pid: u32) -> Result<()> {
    let bytes = read_proc(&format!("/proc/{pid}/status"), 65536)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| Error::new(Kind::InvalidProcess))?;
    for field in ["Uid:", "Gid:"] {
        let values: Vec<_> = text
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .ok_or_else(|| Error::new(Kind::InvalidProcess))?
            .split_whitespace()
            .collect();
        if values != ["1500"; 4] {
            return Err(Error::new(Kind::InvalidProcess));
        }
    }
    for (field, value) in [("Groups:", ""), ("NoNewPrivs:", "1"), ("Threads:", "1")] {
        if text
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .map(str::trim)
            != Some(value)
        {
            return Err(Error::new(Kind::InvalidProcess));
        }
    }
    Ok(())
}

pub struct ListenerAdmission {
    pub(crate) listener: TcpListener,
    pub(crate) witnesses: Arc<ListenerWitnesses>,
    pub(crate) binding: ConnectBinding,
    pub(crate) port: u16,
}
pub(crate) struct ListenerWitnesses {
    pub(crate) process: ProcessWitness,
    _user: NamespaceFd,
    pub(crate) network: NamespaceFd,
    pub(crate) cookie: u64,
}
#[cfg(test)]
pub(crate) fn test_witness(child: &mut Child) -> Arc<ListenerWitnesses> {
    let process = ProcessWitness::capture(child).unwrap();
    let user = NamespaceFd::current(NamespaceKind::User).unwrap();
    let network = thread_network().unwrap();
    process.check_namespace(&user, NamespaceKind::User).unwrap();
    process
        .check_namespace(&network, NamespaceKind::Network)
        .unwrap();
    Arc::new(ListenerWitnesses {
        process,
        _user: user,
        network,
        cookie: 1,
    })
}
impl ListenerAdmission {
    /// The descriptor vector must be the exact SCM_RIGHTS message from an owned
    /// child control socket. All received FDs are owned and closed on rejection.
    pub fn receive(
        profile: &ValidatedNetworkProfile,
        child: &mut Child,
        frame: ListenerFrame,
        descriptors: Vec<OwnedFd>,
    ) -> Result<Self> {
        let binding = ConnectBinding::for_profile(profile)?;
        if frame.binding != binding || frame.readiness_nonce != profile.readiness_nonce() {
            return Err(Error::new(Kind::BindingMismatch));
        }
        if descriptors.len() != 3 || frame.port == 0 {
            return Err(Error::new(Kind::InvalidListener));
        }
        let mut fds = descriptors.into_iter();
        let user = NamespaceFd::inspect(fds.next().unwrap(), NamespaceKind::User)
            .map_err(|_| Error::new(Kind::InvalidNamespace))?;
        let network = NamespaceFd::inspect(fds.next().unwrap(), NamespaceKind::Network)
            .map_err(|_| Error::new(Kind::InvalidNamespace))?;
        verify_namespace_owner(&user, &network).map_err(|_| Error::new(Kind::InvalidNamespace))?;
        if user.identity()
            == NamespaceFd::current(NamespaceKind::User)
                .map_err(|_| Error::new(Kind::InvalidNamespace))?
                .identity()
            || network.identity() == thread_network()?.identity()
        {
            return Err(Error::new(Kind::InvalidNamespace));
        }
        let process = ProcessWitness::capture(child)?;
        process.check_namespace(&user, NamespaceKind::User)?;
        process.check_namespace(&network, NamespaceKind::Network)?;
        cloud_identity(process.pid)?;
        let fd = fds.next().unwrap();
        inspect_listener(fd.as_raw_fd(), frame.port)?;
        // Requires CAP_NET_ADMIN in the USER owning this particular socket NET.
        // Ancestor/child capability semantics are qualified, never guessed.
        let socket_net = socket_namespace(fd.as_raw_fd())?;
        verify_namespace_owner(&user, &socket_net)
            .map_err(|_| Error::new(Kind::InvalidNamespace))?;
        if socket_net.identity() != network.identity()
            || cookie(fd.as_raw_fd())? != frame.socket_cookie
        {
            return Err(Error::new(Kind::InvalidListener));
        }
        process.check_namespace(&user, NamespaceKind::User)?;
        process.check_namespace(&network, NamespaceKind::Network)?;
        cloud_identity(process.pid)?;
        Ok(Self {
            listener: TcpListener::from(fd),
            port: frame.port,
            binding,
            witnesses: Arc::new(ListenerWitnesses {
                process,
                _user: user,
                network,
                cookie: frame.socket_cookie,
            }),
        })
    }
    pub fn port(&self) -> u16 {
        self.port
    }
}

fn inspect_listener(fd: RawFd, port: u16) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0
        || unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFSOCK
    {
        return Err(Error::new(Kind::InvalidListener));
    }
    for (option, expected) in [
        (libc::SO_DOMAIN, libc::AF_INET),
        (libc::SO_TYPE, libc::SOCK_STREAM),
        (libc::SO_PROTOCOL, libc::IPPROTO_TCP),
        (libc::SO_ACCEPTCONN, 1),
    ] {
        if option_i32(fd, option)? != expected {
            return Err(Error::new(Kind::InvalidListener));
        }
    }
    set_nonblocking_cloexec(fd)?;
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut length = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    if unsafe { libc::getsockname(fd, address.as_mut_ptr().cast(), &mut length) } != 0
        || length as usize != std::mem::size_of::<libc::sockaddr_in>()
    {
        return Err(Error::new(Kind::InvalidListener));
    }
    let address = unsafe { address.assume_init() };
    if i32::from(address.sin_family) != libc::AF_INET
        || address.sin_addr.s_addr != u32::from_ne_bytes([127, 0, 0, 1])
        || u16::from_be(address.sin_port) != port
    {
        return Err(Error::new(Kind::InvalidListener));
    }
    let mut peer = std::mem::MaybeUninit::<libc::sockaddr_storage>::uninit();
    let mut size = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    if unsafe { libc::getpeername(fd, peer.as_mut_ptr().cast(), &mut size) } == 0
        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOTCONN)
    {
        return Err(Error::new(Kind::InvalidListener));
    }
    Ok(())
}
pub(crate) fn set_nonblocking_cloexec(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0
        || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0
    {
        return Err(Error::os(Kind::Io));
    }
    Ok(())
}
pub(crate) fn option_i32(fd: RawFd, option: i32) -> Result<i32> {
    let mut value = 0i32;
    let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut i32).cast(),
            &mut size,
        )
    } != 0
        || size as usize != std::mem::size_of::<i32>()
    {
        return Err(Error::os(Kind::Io));
    }
    Ok(value)
}
pub(crate) fn cookie(fd: RawFd) -> Result<u64> {
    let mut value = 0u64;
    let mut size = std::mem::size_of::<u64>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_NETNS_COOKIE,
            (&mut value as *mut u64).cast(),
            &mut size,
        )
    } != 0
        || size as usize != std::mem::size_of::<u64>()
        || value == 0
    {
        return Err(Error::os(Kind::InvalidListener));
    }
    Ok(value)
}
pub(crate) fn socket_namespace(fd: RawFd) -> Result<NamespaceFd> {
    let net = unsafe { libc::ioctl(fd, SIOCGSKNS) };
    if net < 0 {
        return Err(Error::os(Kind::SocketNamespaceUnavailable));
    }
    NamespaceFd::inspect(unsafe { OwnedFd::from_raw_fd(net) }, NamespaceKind::Network)
        .map_err(|_| Error::new(Kind::InvalidNamespace))
}
pub(crate) fn thread_network() -> Result<NamespaceFd> {
    let fd =
        File::open("/proc/thread-self/ns/net").map_err(|_| Error::new(Kind::InvalidNamespace))?;
    NamespaceFd::inspect(fd.into(), NamespaceKind::Network)
        .map_err(|_| Error::new(Kind::InvalidNamespace))
}

/// No setns: each upstream and resolver worker remains in this exact held host
/// NET. Newly created host sockets are checked against its kernel cookie too.
pub struct HostNetworkAnchor {
    namespace: NamespaceFd,
    cookie: u64,
}
impl HostNetworkAnchor {
    pub fn capture() -> Result<Self> {
        let namespace = thread_network()?;
        let fd = new_tcp_socket()?;
        let value = Self {
            namespace,
            cookie: cookie(fd.as_raw_fd())?,
        };
        value.check_current()?;
        Ok(value)
    }
    pub(crate) fn check_current(&self) -> Result<()> {
        if self.namespace.identity() != thread_network()?.identity() {
            return Err(Error::new(Kind::InvalidNamespace));
        }
        Ok(())
    }
    pub(crate) fn check_process(&self, process: &ProcessWitness) -> Result<()> {
        process.check_namespace(&self.namespace, NamespaceKind::Network)
    }
    pub(crate) fn check_socket(&self, socket: &TcpStream) -> Result<()> {
        self.check_current()?;
        if cookie(socket.as_raw_fd())? != self.cookie {
            return Err(Error::new(Kind::InvalidNamespace));
        }
        Ok(())
    }
}
pub(crate) fn new_tcp_socket() -> Result<OwnedFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_INET,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            libc::IPPROTO_TCP,
        )
    };
    if fd < 0 {
        return Err(Error::os(Kind::Io));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_listener_and_wrong_port_without_namespace_ioctl() {
        let file = File::open("/dev/null").unwrap();
        assert_eq!(
            inspect_listener(file.as_raw_fd(), 1).unwrap_err().kind(),
            Kind::InvalidListener
        );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(inspect_listener(listener.as_raw_fd(), port).is_ok());
        assert!(inspect_listener(
            listener.as_raw_fd(),
            if port == 65535 { 1 } else { port + 1 }
        )
        .is_err());
    }
    #[test]
    fn held_host_anchor_checks_actual_thread_and_socket() {
        let anchor = HostNetworkAnchor::capture().unwrap();
        let socket = TcpStream::from(new_tcp_socket().unwrap());
        anchor.check_socket(&socket).unwrap();
        // The current ordinary file is rejected by NamespaceFd before ioctl.
        assert!(NamespaceFd::inspect(
            File::open("/dev/null").unwrap().into(),
            NamespaceKind::Network
        )
        .is_err());
    }
}
