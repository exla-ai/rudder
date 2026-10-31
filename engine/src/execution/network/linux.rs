//! Child-only Linux namespace/capability evidence. No live owner authorization,
//! scheduling, host networking, egress or privileged device provisioning.
use super::{NetworkError, NetworkErrorKind as Kind, NetworkResult, ValidatedNetworkProfile};
use std::{
    ffi::CStr,
    marker::PhantomData,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    rc::Rc,
};
use uuid::Uuid;

const NS_GET_USERNS: libc::c_ulong = libc::NS_GET_USERNS as libc::c_ulong;
const NS_GET_NSTYPE: libc::c_ulong = libc::NS_GET_NSTYPE as libc::c_ulong;
const CAPABILITY_V3: u32 = 0x2008_0522;
const CAPABILITY_BITS: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceKind {
    User,
    Network,
}
impl NamespaceKind {
    fn flag(self) -> libc::c_int {
        match self {
            Self::User => libc::CLONE_NEWUSER,
            Self::Network => libc::CLONE_NEWNET,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceIdentity {
    device: u64,
    inode: u64,
    kind: NamespaceKind,
}
impl NamespaceIdentity {
    pub fn device(self) -> u64 {
        self.device
    }
    pub fn inode(self) -> u64 {
        self.inode
    }
    pub fn kind(self) -> NamespaceKind {
        self.kind
    }
}

/// An inspected, held kernel FD, not proof of which helper sent it or current
/// attempt authorization. No constructor from serialized inode numbers.
pub struct NamespaceFd {
    fd: OwnedFd,
    identity: NamespaceIdentity,
}
impl NamespaceFd {
    pub fn inspect(fd: OwnedFd, kind: NamespaceKind) -> NetworkResult<Self> {
        let actual = unsafe { libc::ioctl(fd.as_raw_fd(), NS_GET_NSTYPE) };
        if actual != kind.flag() {
            return Err(NetworkError::new(Kind::InvalidNamespaceFd));
        }
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC == 0
        {
            return Err(NetworkError::os(Kind::InvalidNamespaceFd));
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(NetworkError::os(Kind::InvalidNamespaceFd));
        }
        let stat = unsafe { stat.assume_init() };
        Ok(Self {
            fd,
            identity: NamespaceIdentity {
                device: stat.st_dev as u64,
                inode: stat.st_ino as u64,
                kind,
            },
        })
    }
    pub fn identity(&self) -> NamespaceIdentity {
        self.identity
    }
    fn current(kind: NamespaceKind) -> NetworkResult<Self> {
        let path = match kind {
            NamespaceKind::User => c"/proc/self/ns/user",
            NamespaceKind::Network => c"/proc/self/ns/net",
        };
        Self::inspect(open(path, libc::O_RDONLY)?, kind)
    }
}
impl AsFd for NamespaceFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Captured in the actual fresh Cloud UID1500 child before its USER/MNT entry.
/// The public profile/expected data never authenticates that child or its host.
pub struct ChildNetworkBaseline {
    user: NamespaceFd,
    network: NamespaceFd,
    pid: libc::pid_t,
    network_id: Uuid,
    profile_digest: [u8; 32],
    _same_thread: PhantomData<Rc<()>>,
}
impl ChildNetworkBaseline {
    pub fn capture(profile: &ValidatedNetworkProfile) -> NetworkResult<Self> {
        fresh_cloud_child()?;
        Ok(Self {
            user: NamespaceFd::current(NamespaceKind::User)?,
            network: NamespaceFd::current(NamespaceKind::Network)?,
            pid: unsafe { libc::getpid() },
            network_id: profile.network_id(),
            profile_digest: profile.profile_digest(),
            _same_thread: PhantomData,
        })
    }
    /// Reuse the existing fresh USER/MNT transition; never create another USER
    /// after that mount namespace was established. Mapping/NET witnesses must
    /// still belong to this exact process. Consumes and verifies closure of the
    /// parent witnesses BEFORE returning, before any untrusted PID tree exists.
    /// Errors require helper exit; CLOEXEC alone is not this lifetime barrier.
    ///
    /// # Safety
    /// Caller is an owned fresh helper, not App/host execution; namespace changes
    /// are irreversible. No untrusted code may run on success or error before
    /// the separate privilege and current-attempt admission barriers.
    pub unsafe fn enter_after_new_user(self) -> NetworkResult<OwnedChildNetwork> {
        fresh_cloud_child()?;
        if unsafe { libc::getpid() } != self.pid {
            return Err(NetworkError::new(Kind::WrongProcess));
        }
        let user = NamespaceFd::current(NamespaceKind::User)?;
        let before_network = NamespaceFd::current(NamespaceKind::Network)?;
        if user.identity == self.user.identity || before_network.identity != self.network.identity {
            return Err(NetworkError::new(Kind::ForeignNamespace));
        }
        drop(before_network);
        exact_mapping(c"/proc/self/uid_map")?;
        exact_mapping(c"/proc/self/gid_map")?;
        let groups = open(c"/proc/self/setgroups", libc::O_RDONLY)?;
        let mut value = [0u8; 6];
        let count =
            unsafe { libc::read(groups.as_raw_fd(), value.as_mut_ptr().cast(), value.len()) };
        if count != 5 || &value[..5] != b"deny\n" {
            return Err(NetworkError::new(Kind::ForeignNamespace));
        }
        if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
            return Err(NetworkError::os(Kind::NamespaceUnavailable));
        }
        let network = NamespaceFd::current(NamespaceKind::Network)?;
        if network.identity == self.network.identity {
            return Err(NetworkError::new(Kind::ForeignNamespace));
        }
        verify_owner(&user, &network)?;
        bring_loopback_up()?;
        let network_id = self.network_id;
        let profile_digest = self.profile_digest;
        let parent_fds = [self.user.fd.as_raw_fd(), self.network.fd.as_raw_fd()];
        drop(self.user);
        drop(self.network);
        // Fresh single-threaded helper: no concurrent FD reuse is permitted.
        // Never return a current-child witness while retaining a host witness.
        for fd in parent_fds {
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1
                || std::io::Error::last_os_error().raw_os_error() != Some(libc::EBADF)
            {
                return Err(NetworkError::new(Kind::ParentWitnessRetained));
            }
        }
        Ok(OwnedChildNetwork {
            user,
            network,
            network_id,
            profile_digest,
        })
    }
    /// Standalone qualification only. Production's future mount composition uses
    /// enter_after_new_user instead; this does not create a private mount/root.
    ///
    /// # Safety
    /// Same owned fresh-child/exit-on-error requirements as enter_after_new_user.
    pub unsafe fn enter_new_user_and_net_for_qualification(
        self,
    ) -> NetworkResult<OwnedChildNetwork> {
        fresh_cloud_child()?;
        if unsafe { libc::getpid() } != self.pid {
            return Err(NetworkError::new(Kind::WrongProcess));
        }
        if NamespaceFd::current(NamespaceKind::User)?.identity != self.user.identity
            || NamespaceFd::current(NamespaceKind::Network)?.identity != self.network.identity
        {
            return Err(NetworkError::new(Kind::ForeignNamespace));
        }
        if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
            return Err(NetworkError::os(Kind::NamespaceUnavailable));
        }
        write_fixed(c"/proc/self/setgroups", b"deny\n")?;
        write_fixed(c"/proc/self/uid_map", b"1500 1500 1\n")?;
        write_fixed(c"/proc/self/gid_map", b"1500 1500 1\n")?;
        unsafe { self.enter_after_new_user() }
    }
}

/// Held physical resources only, not a live owner/run/egress grant. Dropping
/// closes references; it neither restores the old NET nor proves helper drain.
pub struct OwnedChildNetwork {
    user: NamespaceFd,
    network: NamespaceFd,
    network_id: Uuid,
    profile_digest: [u8; 32],
}
impl OwnedChildNetwork {
    /// Only current child USER/NET namespace witnesses. Wiring must explicitly
    /// allowlist other init control FDs and close all authority FDs for backend
    /// exec; this list never includes a baseline/host namespace descriptor.
    pub fn child_namespace_fds(&self) -> [BorrowedFd<'_>; 2] {
        [self.user.as_fd(), self.network.as_fd()]
    }
    pub fn user_namespace(&self) -> &NamespaceFd {
        &self.user
    }
    pub fn network_namespace(&self) -> &NamespaceFd {
        &self.network
    }
    pub fn network_id(&self) -> Uuid {
        self.network_id
    }
    pub fn profile_digest(&self) -> [u8; 32] {
        self.profile_digest
    }
    pub fn verify_current(&self) -> NetworkResult<()> {
        if NamespaceFd::current(NamespaceKind::User)?.identity != self.user.identity
            || NamespaceFd::current(NamespaceKind::Network)?.identity != self.network.identity
        {
            return Err(NetworkError::new(Kind::ForeignNamespace));
        }
        verify_owner(&self.user, &self.network)?;
        let control = network_control()?;
        if loopback_flags(&control)? & libc::IFF_UP as libc::c_short == 0 {
            return Err(NetworkError::new(Kind::LoopbackSetupFailed));
        }
        Ok(())
    }
}

fn verify_owner(user: &NamespaceFd, network: &NamespaceFd) -> NetworkResult<()> {
    let owner = unsafe { libc::ioctl(network.fd.as_raw_fd(), NS_GET_USERNS) };
    if owner < 0 {
        return Err(NetworkError::os(Kind::ForeignNamespace));
    }
    let owner = unsafe { OwnedFd::from_raw_fd(owner) };
    let owner = NamespaceFd::inspect(owner, NamespaceKind::User)?;
    if owner.identity != user.identity {
        return Err(NetworkError::new(Kind::ForeignNamespace));
    }
    Ok(())
}
fn open(path: &CStr, flags: libc::c_int) -> NetworkResult<OwnedFd> {
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(NetworkError::os(Kind::NamespaceUnavailable));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
fn write_fixed(path: &CStr, bytes: &[u8]) -> NetworkResult<()> {
    let fd = open(path, libc::O_WRONLY)?;
    if unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) }
        != bytes.len() as isize
    {
        return Err(NetworkError::os(Kind::NamespaceUnavailable));
    }
    Ok(())
}
fn exact_mapping(path: &CStr) -> NetworkResult<()> {
    let fd = open(path, libc::O_RDONLY)?;
    let mut bytes = [0u8; 128];
    let count = unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
    if count < 0 {
        return Err(NetworkError::os(Kind::ForeignNamespace));
    }
    if count == 0 || count as usize == bytes.len() {
        return Err(NetworkError::new(Kind::ForeignNamespace));
    }
    let mut extra = 0u8;
    if unsafe { libc::read(fd.as_raw_fd(), (&mut extra as *mut u8).cast(), 1) } != 0 {
        return Err(NetworkError::new(Kind::ForeignNamespace));
    }
    let bytes = &bytes[..count as usize];
    let text = std::str::from_utf8(bytes).map_err(|_| NetworkError::new(Kind::ForeignNamespace))?;
    let mut fields = text.split_ascii_whitespace();
    if fields.next() != Some("1500")
        || fields.next() != Some("1500")
        || fields.next() != Some("1")
        || fields.next().is_some()
    {
        return Err(NetworkError::new(Kind::ForeignNamespace));
    }
    Ok(())
}
fn fresh_cloud_child() -> NetworkResult<()> {
    let mut real = 0;
    let mut effective = 0;
    let mut saved = 0;
    if unsafe { libc::getresuid(&mut real, &mut effective, &mut saved) } != 0
        || [real, effective, saved] != [1500; 3]
    {
        return Err(NetworkError::new(Kind::WrongIdentity));
    }
    if unsafe { libc::getresgid(&mut real, &mut effective, &mut saved) } != 0
        || [real, effective, saved] != [1500; 3]
        || unsafe { libc::getgroups(0, std::ptr::null_mut()) } != 0
    {
        return Err(NetworkError::new(Kind::WrongIdentity));
    }
    if unsafe { libc::getpid() } != unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t {
        return Err(NetworkError::new(Kind::NotFreshChild));
    }
    if unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
        return Err(NetworkError::new(Kind::WrongIdentity));
    }
    let fd = open(c"/proc/self/task", libc::O_RDONLY | libc::O_DIRECTORY)?;
    let mut bytes = [0u8; 4096];
    let mut tasks = 0;
    // getdents64 uses stack storage, avoiding allocator/directory-stream state.
    for _ in 0..2 {
        let count = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                fd.as_raw_fd(),
                bytes.as_mut_ptr(),
                bytes.len(),
            )
        };
        if count < 0 {
            return Err(NetworkError::os(Kind::NotFreshChild));
        }
        if count == 0 {
            return if tasks == 1 {
                Ok(())
            } else {
                Err(NetworkError::new(Kind::NotFreshChild))
            };
        }
        let count = count as usize;
        let mut offset = 0;
        while offset < count {
            if offset + 20 > count {
                return Err(NetworkError::new(Kind::NotFreshChild));
            }
            let length = u16::from_ne_bytes([bytes[offset + 16], bytes[offset + 17]]) as usize;
            if length < 20 || offset + length > count {
                return Err(NetworkError::new(Kind::NotFreshChild));
            }
            let name = &bytes[offset + 19..offset + length];
            let end = name
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| NetworkError::new(Kind::NotFreshChild))?;
            if &name[..end] != b"." && &name[..end] != b".." {
                tasks += 1;
            }
            if tasks > 1 {
                return Err(NetworkError::new(Kind::NotFreshChild));
            }
            offset += length;
        }
    }
    Err(NetworkError::new(Kind::NotFreshChild))
}

fn network_control() -> NetworkResult<OwnedFd> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(NetworkError::os(Kind::LoopbackSetupFailed));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
fn loopback_request() -> libc::ifreq {
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    request.ifr_name[0] = b'l' as libc::c_char;
    request.ifr_name[1] = b'o' as libc::c_char;
    request
}
fn loopback_flags(control: &OwnedFd) -> NetworkResult<libc::c_short> {
    let mut request = loopback_request();
    if unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCGIFFLAGS, &mut request) } != 0 {
        return Err(NetworkError::os(Kind::LoopbackSetupFailed));
    }
    Ok(unsafe { request.ifr_ifru.ifru_flags })
}
fn bring_loopback_up() -> NetworkResult<()> {
    let control = network_control()?;
    let flags = loopback_flags(&control)?;
    let mut request = loopback_request();
    request.ifr_ifru.ifru_flags = flags | libc::IFF_UP as libc::c_short;
    if unsafe { libc::ioctl(control.as_raw_fd(), libc::SIOCSIFFLAGS, &request) } != 0 {
        return Err(NetworkError::os(Kind::LoopbackSetupFailed));
    }
    if loopback_flags(&control)? & libc::IFF_UP as libc::c_short == 0 {
        return Err(NetworkError::new(Kind::LoopbackSetupFailed));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilitySnapshot {
    pub effective: u64,
    pub permitted: u64,
    pub inheritable: u64,
    pub ambient: u64,
    pub bounding: u64,
    pub no_new_privileges: bool,
    pub supported_bits: u32,
}
impl CapabilitySnapshot {
    pub fn all_five_zero(self) -> bool {
        self.effective == 0
            && self.permitted == 0
            && self.inheritable == 0
            && self.ambient == 0
            && self.bounding == 0
    }
}
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}
#[derive(Clone, Copy)]
#[repr(C)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

pub fn read_capabilities() -> NetworkResult<CapabilitySnapshot> {
    let mut header = CapHeader {
        version: CAPABILITY_V3,
        pid: 0,
    };
    let mut data = [CapData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    if unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } != 0
        || header.version != CAPABILITY_V3
    {
        return Err(NetworkError::os(Kind::CapabilityReadFailed));
    }
    let combine = |low: u32, high: u32| u64::from(low) | u64::from(high) << 32;
    let mut snapshot = CapabilitySnapshot {
        effective: combine(data[0].effective, data[1].effective),
        permitted: combine(data[0].permitted, data[1].permitted),
        inheritable: combine(data[0].inheritable, data[1].inheritable),
        ambient: 0,
        bounding: 0,
        no_new_privileges: false,
        supported_bits: 0,
    };
    for bit in 0..=CAPABILITY_BITS {
        let bound = unsafe { libc::prctl(libc::PR_CAPBSET_READ, bit as libc::c_ulong, 0, 0, 0) };
        if bound < 0 {
            if bit == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL) {
                return Err(NetworkError::os(Kind::CapabilityReadFailed));
            }
            snapshot.supported_bits = bit;
            break;
        }
        if bit == CAPABILITY_BITS || ![0, 1].contains(&bound) {
            return Err(NetworkError::new(Kind::CapabilityReadFailed));
        }
        let ambient = unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_IS_SET as libc::c_ulong,
                bit as libc::c_ulong,
                0,
                0,
            )
        };
        if ![0, 1].contains(&ambient) {
            return Err(NetworkError::os(Kind::CapabilityReadFailed));
        }
        snapshot.bounding |= (bound as u64) << bit;
        snapshot.ambient |= (ambient as u64) << bit;
    }
    let mask = u64::MAX
        .checked_shr(CAPABILITY_BITS - snapshot.supported_bits)
        .unwrap_or(0);
    if snapshot.supported_bits == 0
        || (snapshot.effective
            | snapshot.permitted
            | snapshot.inheritable
            | snapshot.ambient
            | snapshot.bounding)
            & !mask
            != 0
    {
        return Err(NetworkError::new(Kind::CapabilityReadFailed));
    }
    let nnp = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
    if ![0, 1].contains(&nnp) {
        return Err(NetworkError::os(Kind::CapabilityReadFailed));
    }
    snapshot.no_new_privileges = nnp == 1;
    Ok(snapshot)
}

/// Current thread evidence only. Neither Clone/Send/Deserialize nor a substitute
/// for namespace ownership, broker authentication or current-attempt authority.
pub struct CapabilityDropProof {
    pid: libc::pid_t,
    tid: libc::pid_t,
    snapshot: CapabilitySnapshot,
    _same_thread: PhantomData<Rc<()>>,
}
impl CapabilityDropProof {
    pub fn snapshot(&self) -> CapabilitySnapshot {
        self.snapshot
    }
    pub fn verify_current_for_exec(&self) -> NetworkResult<()> {
        if unsafe { libc::getpid() } != self.pid
            || unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t != self.tid
        {
            return Err(NetworkError::new(Kind::WrongProcess));
        }
        let current = read_capabilities()?;
        if !current.all_five_zero() || !current.no_new_privileges {
            return Err(NetworkError::new(Kind::CapabilitiesRemain));
        }
        Ok(())
    }
}

/// Drop ambient, then every supported bounding bit while CAP_SETPCAP remains
/// effective, then capset0/NNP and verify all five sets on this exact thread.
/// No external program is used and no already-saved receipt can mint the proof.
///
/// # Safety
/// Irreversibly changes the calling thread. Call only in the owned execution
/// child after the last namespace/privileged setup and before untrusted exec.
/// On any error, withhold exec and exit that helper; there is no fallback.
pub unsafe fn drop_all_capabilities_for_exec() -> NetworkResult<CapabilityDropProof> {
    let before = read_capabilities()?;
    if unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
            0,
            0,
            0,
        )
    } != 0
    {
        return Err(NetworkError::os(Kind::CapabilityDropFailed));
    }
    for bit in 0..before.supported_bits {
        if before.bounding & (1u64 << bit) != 0
            && unsafe { libc::prctl(libc::PR_CAPBSET_DROP, bit as libc::c_ulong, 0, 0, 0) } != 0
        {
            return Err(NetworkError::os(Kind::CapabilityDropFailed));
        }
    }
    let header = CapHeader {
        version: CAPABILITY_V3,
        pid: 0,
    };
    let data = [CapData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    if unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } != 0
        || unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) } != 0
    {
        return Err(NetworkError::os(Kind::CapabilityDropFailed));
    }
    let snapshot = read_capabilities()?;
    if !snapshot.all_five_zero() || !snapshot.no_new_privileges {
        return Err(NetworkError::new(Kind::CapabilitiesRemain));
    }
    Ok(CapabilityDropProof {
        pid: unsafe { libc::getpid() },
        tid: unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t,
        snapshot,
        _same_thread: PhantomData,
    })
}
