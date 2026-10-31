//! Explicit qualification-only CONNECT transport. This does not change the
//! managed execution profile, readiness protocol, or offline wire format.
//! Configuration, live authority, listener custody and actual settlement are
//! deliberately separate objects; none can be reconstructed from a report.
use super::{HostIpv4DenySet, NetworkBinding, NetworkMode, ValidatedNetworkProfile};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    os::fd::{AsRawFd, FromRawFd},
    sync::Arc,
    time::Duration,
};

pub mod qualification;
mod relay;
mod resolver;
mod socket;

pub use relay::{ConnectRequest, ConnectedSettlement, OwnedConnectProxy, ProxySnapshot};
pub use resolver::run_resolver_process;
pub use socket::{HostNetworkAnchor, ListenerAdmission, ListenerFrame};

pub const CONNECT_TRANSPORT_VERSION: u32 = 1;
pub const CONNECT_TRANSPORT_PROTOCOL: &str = "rudder_connect_transport_v1";
/// A lifetime quota, including malformed requests and connections with no body.
pub const MAX_LIFETIME_CONNECTIONS: u32 = 32;
pub const MAX_RESOLVER_FRAME: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectErrorKind {
    InvalidConfiguration,
    BindingMismatch,
    AuthorityDenied,
    Cancelled,
    Deadline,
    ResourceLimit,
    InvalidProcess,
    InvalidNamespace,
    InvalidListener,
    SocketNamespaceUnavailable,
    InvalidRequest,
    DestinationDenied,
    ResolverFailed,
    ConnectFailed,
    Io,
    Unsettled,
}
/// Static errors never copy request headers, DNS answers, argv, or paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectError {
    kind: ConnectErrorKind,
    errno: Option<i32>,
}
impl ConnectError {
    pub(crate) fn new(kind: ConnectErrorKind) -> Self {
        Self { kind, errno: None }
    }
    pub(crate) fn os(kind: ConnectErrorKind) -> Self {
        Self {
            kind,
            errno: std::io::Error::last_os_error().raw_os_error(),
        }
    }
    pub fn kind(self) -> ConnectErrorKind {
        self.kind
    }
    pub fn raw_os_error(self) -> Option<i32> {
        self.errno
    }
}
impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "connected network {:?}", self.kind)
    }
}
impl std::error::Error for ConnectError {}
pub type ConnectResult<T> = Result<T, ConnectError>;

/// Exact sealed configuration identity. Matching this data grants no authority.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectBinding {
    pub format_version: u32,
    pub protocol: String,
    pub network: NetworkBinding,
    pub profile_sha256: String,
}
impl ConnectBinding {
    pub fn for_profile(profile: &ValidatedNetworkProfile) -> ConnectResult<Self> {
        if profile.mode() != NetworkMode::QualificationHttpsConnectV1 {
            return Err(ConnectError::new(ConnectErrorKind::InvalidConfiguration));
        }
        Ok(Self {
            format_version: CONNECT_TRANSPORT_VERSION,
            protocol: CONNECT_TRANSPORT_PROTOCOL.into(),
            network: profile.binding().clone(),
            profile_sha256: hex::encode(profile.profile_digest()),
        })
    }
}

/// An actual host-held current grant. A saved binding, cancellation flag or
/// caller-supplied boolean is not an implementation. Implementations must check
/// the current owner/attempt and return a fresh trusted host-address inventory.
/// Calls may outlive a deadline: their fixed worker and owner remain charged
/// until they return, and no settlement receipt is minted in the meantime.
pub trait ConnectAuthority: Send + Sync + 'static {
    fn validate(&self, binding: &ConnectBinding) -> ConnectResult<HostIpv4DenySet>;
}

/// Launch recipe only, not a live resolver or permission. The executable must
/// dispatch `--connected-network-resolver-v1` before normal application startup.
/// No user environment or inherited credentials are passed to that process.
pub struct ResolverProgram {
    pub(crate) executable: Arc<std::fs::File>,
}
impl ResolverProgram {
    pub fn current_executable() -> ConnectResult<Self> {
        // Pin the loaded ELF instead of a pathname that could be replaced.
        let opened = std::fs::File::open("/proc/self/exe")
            .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidConfiguration))?;
        let fd = unsafe { libc::fcntl(opened.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 16) };
        if fd < 0 {
            return Err(ConnectError::os(ConnectErrorKind::InvalidConfiguration));
        }
        let executable = unsafe { std::fs::File::from_raw_fd(fd) };
        drop(opened);
        let metadata = executable
            .metadata()
            .map_err(|_| ConnectError::new(ConnectErrorKind::InvalidConfiguration))?;
        let mut magic = [0u8; 4];
        if !metadata.is_file()
            || metadata.len() > 1024 * 1024 * 1024
            || unsafe { libc::pread(executable.as_raw_fd(), magic.as_mut_ptr().cast(), 4, 0) } != 4
            || &magic != b"\x7fELF"
        {
            return Err(ConnectError::new(ConnectErrorKind::InvalidConfiguration));
        }
        Ok(Self {
            executable: Arc::new(executable),
        })
    }
    pub(crate) fn command(&self) -> std::process::Command {
        std::process::Command::new(format!("/proc/self/fd/{}", self.executable.as_raw_fd()))
    }
}

impl OwnedConnectProxy {
    /// All resources remain owned while startup runs. Call await_ready before
    /// sending the child its admission ACK; start alone is not admission.
    pub fn start(
        profile: ValidatedNetworkProfile,
        listener: ListenerAdmission,
        host: HostNetworkAnchor,
        authority: Arc<dyn ConnectAuthority>,
        resolver: ResolverProgram,
    ) -> ConnectResult<Self> {
        relay::start(profile, listener, host, authority, resolver)
    }
    /// Network endpoint settlement only. It proves neither stopped guest PIDs
    /// nor RPC/FUSE/data durability, and is never a checkpoint authority.
    pub fn close_and_settle(&mut self, timeout: Duration) -> ConnectResult<ConnectedSettlement> {
        self.settle(timeout)
    }
}
