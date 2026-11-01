//! Image-owned Linux root storage only. No scheduler, profiles, user paths,
//! commands, adoption from saved JSON, public socket name or automatic cleanup.
#![cfg(target_os = "linux")]
use anyhow::{ensure, Context, Result};
use crate::root_seed_mount::{RootSeedMount, SeedMountRead};
use rudder_engine::{execution::finalization_budget::{FinalizationBudget, FinalizationConstraint},
    protocol::{canonical_json, ObjectKind, ObjectRef}, store::LocalStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, fs::{self, File, OpenOptions}, io::Read,
    os::{fd::{AsRawFd, FromRawFd, OwnedFd}, unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt}},
    path::{Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};

#[cfg(not(test))]
pub const ENGINE_IMAGE: &str = "/opt/rudder/dist/native/rudder-native";
#[cfg(not(test))]
pub const SERVICE_IMAGE: &str = "/usr/local/bin/rudder-root-storage";
// Only native libtests use this fixed root-owned/hash-pinned ELF. No runtime
// path override exists and production constructors keep their original images.
#[cfg(test)]
pub const ENGINE_IMAGE: &str = "/usr/local/bin/rudder-root-seed-test";
#[cfg(test)]
pub const SERVICE_IMAGE: &str = "/usr/local/bin/rudder-root-seed-test";
const ROOT_DIRECTORY: &str = "/run/rudder-root-storage";
pub const MAX_SEEDS: usize = 2;
pub const MAX_HELPERS_PER_SEED: usize = 256;
pub const MAX_REFS: usize = 4096;
pub const MAX_TRANSFER_BYTES: u64 = 12 * 1024 * 1024;
pub const MAX_OBJECT_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const CHUNK: usize = 32 * 1024;
const MAX_PACKET: usize = CHUNK + 4096;
fn admit(seed: &ObjectRef, refs: usize, bytes: u64) -> Result<()> {
    seed.validate()?;
    ensure!(seed.kind == ObjectKind::Directory && seed.size_bytes <= 256 * 1024 && refs > 0 && refs <= MAX_REFS
        && bytes > 0 && bytes <= MAX_TRANSFER_BYTES, "storage seed transfer admission exceeds bounds"); Ok(())
}
fn all_helpers_exited(helpers: &BTreeMap<uuid::Uuid, Process>) -> Result<bool> {
    ensure!(!helpers.is_empty(), "storage original helper-reader set unknown");
    for helper in helpers.values() { if !helper.exited()? { return Ok(false); } }
    Ok(true)
}
pub(crate) fn object_bound(reference: &ObjectRef, remaining: u64) -> Result<()> {
    reference.validate()?;
    ensure!(matches!(reference.kind, ObjectKind::Directory | ObjectKind::DirectoryShard | ObjectKind::File | ObjectKind::Blob)
        && reference.size_bytes <= MAX_OBJECT_BYTES && reference.size_bytes <= remaining
        && match reference.kind { ObjectKind::Directory => reference.size_bytes <= 256 * 1024,
            ObjectKind::DirectoryShard | ObjectKind::File => reference.size_bytes <= 64 * 1024, _ => true },
        "storage typed object exceeds original admission"); Ok(())
}

pub(crate) fn fd_cloexec(fd: &OwnedFd) -> Result<()> {
    ensure!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0, "storage descriptor custody unavailable"); Ok(())
}
pub(crate) fn proc_bytes(path: &str, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK).open(path)?;
    let mut kind: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(unsafe { libc::fstatfs(file.as_raw_fd(), &mut kind) } == 0 && kind.f_type == 0x9fa0, "storage procfs unavailable");
    let mut bytes = Vec::new(); file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "storage procfs exceeds bound"); Ok(bytes)
}
pub(crate) fn id(file: &File) -> Result<(u64, u64)> { let m = file.metadata()?; Ok((m.dev(), m.ino())) }
struct RootDirectory { path: PathBuf, file: File, identity: (u64, u64) }
impl RootDirectory {
    fn capture(path: &Path) -> Result<Self> {
        ensure!(path.canonicalize()? == path, "storage root directory redirects");
        let file = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
        let owner = Self { path: path.into(), identity: id(&file)?, file }; owner.check()?; Ok(owner)
    }
    fn check(&self) -> Result<()> {
        for m in [self.file.metadata()?, fs::symlink_metadata(&self.path)?] {
            ensure!(m.is_dir() && !m.file_type().is_symlink() && (m.dev(), m.ino()) == self.identity
                && m.uid() == 0 && m.gid() == 0 && m.mode() & 0o7777 == 0o700, "storage root directory custody changed");
        } Ok(())
    }
}
pub(crate) fn namespace(pid: Option<u32>, name: &str, kind: i32) -> Result<File> {
    let path = match pid { Some(pid) => format!("/proc/{pid}/ns/{name}"), None => format!("/proc/thread-self/ns/{name}") };
    let file = OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC).open(path)?;
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(unsafe { libc::fstatfs(file.as_raw_fd(), &mut fs) } == 0 && fs.f_type == 0x6e736673
        && unsafe { libc::ioctl(file.as_raw_fd(), libc::NS_GET_NSTYPE as libc::c_ulong) } == kind,
        "storage namespace descriptor invalid"); Ok(file)
}
pub(crate) struct Process { fd: OwnedFd, pub(crate) pid: u32 }
impl Process {
    pub(crate) fn capture(fd: OwnedFd) -> Result<Self> {
        fd_cloexec(&fd)?;
        ensure!(fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))? == Path::new("anon_inode:[pidfd]"), "storage requires actual pidfd");
        let info = String::from_utf8(proc_bytes(&format!("/proc/self/fdinfo/{}", fd.as_raw_fd()), 4096)?)?;
        let pid: u32 = info.lines().find_map(|l| l.strip_prefix("Pid:\t")).context("storage pidfd PID unavailable")?.parse()?;
        ensure!(pid > 1, "storage pidfd is not a live process");
        let value = Self { fd, pid }; value.live()?; Ok(value)
    }
    pub(crate) fn current() -> Result<Self> {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0) } as i32;
        ensure!(raw >= 0, "storage self pidfd unavailable"); Self::capture(unsafe { OwnedFd::from_raw_fd(raw) })
    }
    pub(crate) fn exited(&self) -> Result<bool> {
        let mut p = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut p, 1, 0) };
        ensure!(n >= 0 && p.revents & (libc::POLLERR | libc::POLLNVAL) == 0, "storage process lifetime uncertain");
        Ok(n > 0 && p.revents & libc::POLLIN != 0)
    }
    pub(crate) fn live(&self) -> Result<()> { ensure!(!self.exited()?, "storage original process exited"); Ok(()) }
    pub(crate) fn fd(&self) -> &OwnedFd { &self.fd }
}
pub(crate) struct Image { path: PathBuf, file: File, identity: (u64, u64), len: u64, ctime: (i64, i64), pub(crate) sha256: String }
impl Image {
    pub(crate) fn capture(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute() && path.canonicalize()? == path, "storage image path not canonical");
        for p in path.parent().context("storage image parent absent")?.ancestors() {
            let m = fs::symlink_metadata(p)?;
            ensure!(m.is_dir() && !m.file_type().is_symlink() && m.uid() == 0 && m.gid() == 0 && m.mode() & 0o022 == 0, "storage image ancestor not root controlled");
        }
        let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
        let m = file.metadata()?;
        ensure!(m.is_file() && m.uid() == 0 && m.gid() == 0 && m.nlink() == 1 && m.mode() & 0o022 == 0
            && m.mode() & 0o005 == 0o005 && m.len() > 0 && m.len() <= 512 * 1024 * 1024, "storage image is not image-owned executable");
        let mut hash = Sha256::new(); let mut bytes = [0; 65536]; let mut total = 0u64;
        loop { let n = file.read(&mut bytes)?; if n == 0 { break; } total += n as u64; ensure!(total <= m.len(), "storage image grew"); hash.update(&bytes[..n]); }
        ensure!(total == m.len(), "storage image size changed");
        let value = Self { path: path.into(), identity: id(&file)?, len: m.len(), ctime: (m.ctime(), m.ctime_nsec()), file,
            sha256: format!("{:x}", hash.finalize()) }; value.check()?; Ok(value)
    }
    pub(crate) fn check(&self) -> Result<()> {
        for p in self.path.parent().context("storage image parent absent")?.ancestors() {
            let m = fs::symlink_metadata(p)?;
            ensure!(m.is_dir() && !m.file_type().is_symlink() && m.uid() == 0 && m.gid() == 0 && m.mode() & 0o022 == 0,
                "storage image ancestor custody changed");
        }
        for m in [self.file.metadata()?, fs::symlink_metadata(&self.path)?] {
            ensure!(m.is_file() && !m.file_type().is_symlink() && (m.dev(), m.ino()) == self.identity && m.uid() == 0 && m.gid() == 0
                && m.nlink() == 1 && m.mode() & 0o022 == 0 && m.len() == self.len && (m.ctime(), m.ctime_nsec()) == self.ctime,
                "storage immutable image custody changed");
        } Ok(())
    }
    pub(crate) fn matches_process(&self, process: &Process) -> Result<()> {
        process.live()?; self.check()?;
        let actual = File::open(format!("/proc/{}/exe", process.pid))?;
        ensure!(id(&actual)? == self.identity, "storage process is not the original image executable"); process.live()
    }
    pub(crate) fn matches_fd(&self, fd: &OwnedFd) -> Result<()> {
        let m = fs::metadata(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
        ensure!(m.is_file() && (m.dev(), m.ino()) == self.identity && m.uid() == 0 && m.gid() == 0
            && m.len() == self.len && (m.ctime(), m.ctime_nsec()) == self.ctime, "storage inherited service image differs"); Ok(())
    }
}
pub(crate) fn uid_identity(pid: u32, uid: u32) -> Result<String> {
    // Current-thread credentials/capabilities can differ from the group leader.
    let path = if pid == std::process::id() { "/proc/thread-self/status".into() } else { format!("/proc/{pid}/status") };
    let status = String::from_utf8(proc_bytes(&path, 16384)?)?;
    for field in ["Uid:", "Gid:"] {
        let ids = status.lines().find_map(|l| l.strip_prefix(field)).context("storage peer identity absent")?;
        ensure!(ids.split_whitespace().count() == 4 && ids.split_whitespace().all(|s| s == uid.to_string()), "storage peer identity changed");
    }
    Ok(status)
}
pub(crate) fn floor(pid: u32, uid: u32) -> Result<()> {
    let status = uid_identity(pid, uid)?;
    if uid == 1500 {
        for field in ["CapEff:", "CapPrm:", "CapInh:", "CapAmb:", "CapBnd:"] {
            let caps = status.lines().find_map(|l| l.strip_prefix(field)).context("storage peer capabilities absent")?.trim();
            ensure!(caps.len() == 16 && caps.bytes().all(|b| b == b'0'), "storage peer capabilities remain");
        }
        ensure!(status.lines().any(|l| l.strip_prefix("NoNewPrivs:").is_some_and(|s| s.trim() == "1"))
            && status.lines().any(|l| l.strip_prefix("Groups:").is_some_and(|s| s.trim().is_empty())), "storage peer privilege floor changed");
    } Ok(())
}

pub(crate) struct Endpoint { fd: OwnedFd }
pub(crate) struct Packet { pub(crate) bytes: Vec<u8>, pub(crate) fds: Vec<OwnedFd>, pub(crate) credentials: libc::ucred }
impl Endpoint {
    pub(crate) fn capture(fd: OwnedFd) -> Result<Self> {
        fd_cloexec(&fd)?;
        let mut ty = 0i32; let mut domain = 0i32; let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        ensure!(unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_TYPE, (&mut ty as *mut i32).cast(), &mut len) } == 0 && ty == libc::SOCK_SEQPACKET,
            "storage requires inherited SEQPACKET endpoint");
        len = std::mem::size_of::<i32>() as libc::socklen_t;
        ensure!(unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_DOMAIN, (&mut domain as *mut i32).cast(), &mut len) } == 0 && domain == libc::AF_UNIX,
            "storage endpoint is not local UNIX");
        let mut peer: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let mut peer_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        ensure!(unsafe { libc::getpeername(fd.as_raw_fd(), (&mut peer as *mut libc::sockaddr_un).cast(), &mut peer_len) } == 0
            && peer.sun_family as i32 == libc::AF_UNIX
            && peer_len as usize >= std::mem::size_of::<libc::sa_family_t>()
            && peer_len as usize <= std::mem::size_of::<libc::sockaddr_un>()
            && (peer_len as usize == std::mem::size_of::<libc::sa_family_t>() || peer.sun_path[0] == 0),
            "storage requires connected non-pathname bootstrap pair");
        // SO_PASSCRED may autobind an abstract address after the first packet.
        // Original inherited custody + pidfds/credentials authenticate the pair;
        // neither an abstract name nor a pathname can create that authority.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        ensure!(flags >= 0 && flags & libc::O_NONBLOCK != 0, "storage endpoint must already be nonblocking");
        let yes = 1i32;
        ensure!(unsafe { libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PASSCRED, (&yes as *const i32).cast(), std::mem::size_of::<i32>() as _) } == 0,
            "storage kernel credentials unavailable"); Ok(Self { fd })
    }
    pub(crate) fn send(&self, bytes: &[u8], fd: Option<&OwnedFd>) -> Result<bool> {
        ensure!(!bytes.is_empty() && bytes.len() <= MAX_PACKET, "storage packet exceeds bound");
        let mut iov = libc::iovec { iov_base: bytes.as_ptr().cast_mut().cast(), iov_len: bytes.len() };
        let mut control = [0usize; 8]; let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov; msg.msg_iovlen = 1;
        if let Some(fd) = fd { unsafe {
            msg.msg_control = control.as_mut_ptr().cast(); msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<i32>() as _) as usize;
            let header = libc::CMSG_FIRSTHDR(&msg); (*header).cmsg_level = libc::SOL_SOCKET; (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as _) as usize;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<i32>(), fd.as_raw_fd());
        } }
        let n = unsafe { libc::sendmsg(self.fd.as_raw_fd(), &msg, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock { return Ok(false); }
        ensure!(n == bytes.len() as isize, "storage packet send uncertain"); Ok(true)
    }
    pub(crate) fn receive(&self) -> Result<Option<Packet>> {
        let mut bytes = vec![0u8; MAX_PACKET]; let mut control = [0usize; 64];
        let mut iov = libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: bytes.len() };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov; msg.msg_iovlen = 1; msg.msg_control = control.as_mut_ptr().cast(); msg.msg_controllen = std::mem::size_of_val(&control);
        let n = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut msg, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock { return Ok(None); }
        ensure!(n >= 0, "storage packet receive uncertain");
        let mut fds = Vec::new(); let mut credentials = None; let mut valid = true;
        // Adopt EVERY received descriptor before rejecting truncation/format so
        // error paths close already-delivered SCM_RIGHTS descriptors as well.
        unsafe { let mut header = libc::CMSG_FIRSTHDR(&msg); while !header.is_null() {
            let base = libc::CMSG_LEN(0) as usize;
            if (*header).cmsg_len < base { valid = false; break; }
            let size = (*header).cmsg_len - base;
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                if size % std::mem::size_of::<i32>() != 0 { valid = false; }
                for i in 0..size / std::mem::size_of::<i32>() {
                    let raw = std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<i32>().add(i));
                    fds.push(OwnedFd::from_raw_fd(raw));
                }
            } else if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_CREDENTIALS
                && size == std::mem::size_of::<libc::ucred>() && credentials.is_none() {
                credentials = Some(std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<libc::ucred>()));
            } else { valid = false; }
            header = libc::CMSG_NXTHDR(&msg, header);
        } }
        ensure!(n > 0 && valid && fds.len() <= 1 && msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0, "storage packet closed/truncated/invalid");
        bytes.truncate(n as usize); Ok(Some(Packet { bytes, fds, credentials: credentials.context("storage sender credentials absent")? }))
    }
}
/// Root bootstrap only; neither endpoint is a pathname or a public listener.
pub fn bootstrap_socket_pair() -> Result<(OwnedFd, OwnedFd)> {
    ensure!(unsafe { libc::geteuid() } == 0, "storage pair requires image root bootstrap");
    let mut raw = [-1; 2];
    ensure!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0, raw.as_mut_ptr()) } == 0, "storage socket pair unavailable");
    let first = unsafe { OwnedFd::from_raw_fd(raw[0]) }; let second = unsafe { OwnedFd::from_raw_fd(raw[1]) };
    // Arm credential delivery before either side can enqueue its first packet.
    for fd in [&first, &second] { let yes = 1i32; ensure!(unsafe { libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PASSCRED,
        (&yes as *const i32).cast(), std::mem::size_of::<i32>() as _) } == 0, "storage bootstrap credentials unavailable"); }
    Ok((first, second))
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    Hello { constraint: FinalizationConstraint },
    Begin { constraint: FinalizationConstraint, seed: ObjectRef, refs: usize, bytes: u64 },
    BeginPaged { constraint: FinalizationConstraint, seed: ObjectRef, refs: usize, bytes: u64 },
    CreateMetadataCow { #[serde(with="uuid_wire")] slot:uuid::Uuid, nonce:String, binding:rudder_vcs_bridge::AttemptBinding },
    ArchiveMetadataCow { #[serde(with="uuid_wire")] slot:uuid::Uuid, nonce:String, request:String },
    Object { #[serde(with = "uuid_wire")] slot: uuid::Uuid, reference: ObjectRef },
    PagedInventory { #[serde(with="uuid_wire")] slot:uuid::Uuid,references:Vec<ObjectRef> },
    PagedChunk { #[serde(with="uuid_wire")] slot:uuid::Uuid,reference:ObjectRef,offset:usize },
    Chunk { #[serde(with = "uuid_wire")] slot: uuid::Uuid, offset: usize },
    Commit { #[serde(with = "uuid_wire")] slot: uuid::Uuid },
    Check { #[serde(with = "uuid_wire")] slot: uuid::Uuid },
    EnrollHelper { #[serde(with = "uuid_wire")] slot: uuid::Uuid },
    EnrollHelperCow { #[serde(with="uuid_wire")] slot:uuid::Uuid,nonce:String },
    Fence { #[serde(with = "uuid_wire")] slot: uuid::Uuid },
    // Only the original authenticated fixed engine can issue these commands.
    // Reader IDs are routing DATA; native actual join custody stays in issuer.
    RetireSeed { #[serde(with="uuid_wire")] slot:uuid::Uuid, #[serde(with="uuid_wire")] retirement:uuid::Uuid, readers:Vec<String> },
    ReleaseSeed { #[serde(with="uuid_wire")] slot:uuid::Uuid, #[serde(with="uuid_wire")] retirement:uuid::Uuid },
}
// Native's pinned UUID dependency has v4, not serde. Keep wire IDs canonical
// without changing package features or treating a parsed ID as authority.
mod uuid_wire {
    use serde::{Deserialize, Deserializer, Serializer};
    pub(super) fn serialize<S: Serializer>(id: &uuid::Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&id.to_string())
    }
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<uuid::Uuid, D::Error> {
        let value = String::deserialize(deserializer)?;
        let id = uuid::Uuid::parse_str(&value).map_err(|_| serde::de::Error::custom("storage wire UUID invalid"))?;
        if id.get_version_num() != 4 || id.to_string() != value { return Err(serde::de::Error::custom("storage wire UUID not canonical v4")); }
        Ok(id)
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope { pub(crate) version: u32, pub(crate) serial: u64,
    pub(crate) constraint: FinalizationConstraint, pub(crate) request: Request }
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Reply {
    Hello { #[serde(with = "uuid_wire")] instance: uuid::Uuid, image_sha256: String },
    Begun { #[serde(with = "uuid_wire")] slot: uuid::Uuid },
    Accepted,
    ObjectPresent,
    PagedMissing { #[serde(with="uuid_wire")] slot:uuid::Uuid,missing:Vec<usize> },
    MetadataCowReady { #[serde(with="uuid_wire")] slot:uuid::Uuid, nonce:String, root:PathBuf, base:ObjectRef },
    MetadataCowArchived { #[serde(with="uuid_wire")] slot:uuid::Uuid, nonce:String, request:String, directory:ObjectRef,hardlinks:ObjectRef },
    SeedRetired { #[serde(with="uuid_wire")] slot:uuid::Uuid, #[serde(with="uuid_wire")] retirement:uuid::Uuid },
    SeedReleased { #[serde(with="uuid_wire")] slot:uuid::Uuid, #[serde(with="uuid_wire")] retirement:uuid::Uuid },
    RetirementPending { #[serde(with="uuid_wire")] slot:uuid::Uuid, #[serde(with="uuid_wire")] retirement:uuid::Uuid },
    HelperEnrolled { #[serde(with = "uuid_wire")] reader: uuid::Uuid, pid: u32 },
    Ready { #[serde(with = "uuid_wire")] slot: uuid::Uuid, seed: ObjectRef, root: PathBuf, objects: PathBuf },
    PagedReady { #[serde(with="uuid_wire")] slot:uuid::Uuid,seed:ObjectRef,root:PathBuf,objects:PathBuf,cow_floor:ObjectRef },
    Rejected,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Answer { pub(crate) version: u32, pub(crate) serial: u64, pub(crate) reply: Reply }
pub(crate) fn encode<T: Serialize>(header: &T, payload: &[u8]) -> Result<Vec<u8>> {
    let header = canonical_json(header)?;
    ensure!(header.len() <= 4092 && payload.len() <= CHUNK, "storage frame bound exceeded");
    let mut bytes = Vec::with_capacity(4 + header.len() + payload.len());
    bytes.extend_from_slice(&(header.len() as u32).to_be_bytes()); bytes.extend(header); bytes.extend_from_slice(payload); Ok(bytes)
}
pub(crate) fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<(T, &[u8])> {
    ensure!(bytes.len() >= 4 && bytes.len() <= MAX_PACKET, "storage frame invalid");
    let n = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
    ensure!(n > 0 && n <= 4092 && n + 4 <= bytes.len() && bytes.len() - n - 4 <= CHUNK, "storage header bound exceeded");
    Ok((serde_json::from_slice(&bytes[4..4+n])?, &bytes[4+n..]))
}

pub(crate) fn eventfd() -> Result<OwnedFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    ensure!(fd >= 0, "storage cancellation event unavailable"); Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
pub(crate) fn cancel_event(fd: &OwnedFd) {
    let one = 1u64; let _ = unsafe { libc::write(fd.as_raw_fd(), (&one as *const u64).cast(), 8) };
}
struct Cancellation {
    event: OwnedFd, flag: Arc<AtomicBool>, stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>, result: Option<std::thread::Result<()>>,
}
impl Cancellation {
    fn capture(event: OwnedFd) -> Result<Self> {
        fd_cloexec(&event)?;
        ensure!(fs::read_link(format!("/proc/self/fd/{}", event.as_raw_fd()))? == Path::new("anon_inode:[eventfd]"), "storage cancellation requires eventfd");
        let flags = unsafe { libc::fcntl(event.as_raw_fd(), libc::F_GETFL) };
        ensure!(flags >= 0 && flags & libc::O_NONBLOCK != 0 && flags & libc::O_ACCMODE == libc::O_RDWR, "storage cancellation event flags invalid");
        Ok(Self { event, flag: Arc::new(AtomicBool::new(false)), stop: Arc::new(AtomicBool::new(false)), worker: None, result: None })
    }
    fn start(&mut self, budget: FinalizationBudget) -> Result<()> {
        let event = self.event.try_clone()?; let flag = self.flag.clone(); let stop = self.stop.clone();
        self.worker = Some(std::thread::Builder::new().name("rudder-root-seed-cancel".into()).spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let mut value = 0u64; let n = unsafe { libc::read(event.as_raw_fd(), (&mut value as *mut u64).cast(), 8) };
                if n == 8 || budget.remaining().is_err() || n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock {
                    budget.cancel(); flag.store(true, Ordering::Release); break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })?); Ok(())
    }
    /// Call after the actual observer join, immediately before a success
    /// linearization. A ready event cannot be missed merely because stop won
    /// the observer's final iteration. Later cancellation remains the native
    /// caller's original pre-effect check, not an atomic future guarantee.
    fn check_success(&self,budget:&FinalizationBudget)->Result<()> {
        let mut p=libc::pollfd{fd:self.event.as_raw_fd(),events:libc::POLLIN,revents:0};
        let n=unsafe{libc::poll(&mut p,1,0)};
        ensure!(n==0&&!self.flag.load(Ordering::Acquire),"original storage operation cancelled or observer uncertain");
        budget.remaining()?;Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() { self.result = Some(worker.join()); }
        ensure!(self.result.as_ref().is_some_and(|r| r.is_ok()), "storage cancellation observer join uncertain"); Ok(())
    }
}

struct Slot {
    directory: PathBuf, directory_owner: Option<RootDirectory>, seed: ObjectRef, refs: usize, bytes: u64, received_bytes: u64,
    received: BTreeSet<ObjectRef>, pending: Option<(ObjectRef, Vec<u8>)>, store: Option<LocalStore>,
    stage: Option<String>, import_pin: String, pin_attempted: bool,
    import_unpin_attempted: bool,
    mount: Option<RootSeedMount>, reader: Option<SeedMountRead>, helpers: BTreeMap<uuid::Uuid, Process>,
    budget: FinalizationBudget, cancel: Cancellation, fenced: bool, published: bool,
    retirement:Option<RootRetirement>,
}
struct RootRetirement { id:uuid::Uuid, original:FinalizationBudget, readers:BTreeSet<uuid::Uuid>,
    import_removed:bool, directory_remove_attempted:bool, directory_removed:bool, committed:bool }
struct RetirementAnswer {serial:u64,digest:Vec<u8>,bytes:Vec<u8>,_retired:Option<Slot>}
impl RetirementAnswer {
    fn replay(&self,serial:u64,request:&[u8],has_fd:bool)->Result<Vec<u8>> {
        ensure!(self.serial==serial&&self.digest==Sha256::digest(request).to_vec()&&!has_fd,"storage retirement replay differs");
        Ok(self.bytes.clone())
    }
}
impl Slot {
    fn current(&self) -> Result<()> {
        ensure!(!self.fenced, "storage original slot fenced");
        if let Some(directory) = &self.directory_owner { directory.check()?; }
        self.budget.remaining()?; Ok(())
    }
    fn fence(&mut self) {
        self.fenced = true; self.cancel.flag.store(true, Ordering::Release); self.budget.cancel();
        if let Some(mount) = &self.mount { mount.close_admission(); }
    }
    fn store(&self) -> Result<&LocalStore> { self.store.as_ref().context("storage original CAS unavailable") }
}
/// No public constructor or Deserialize. Issued only inside authenticated
/// dispatch after the fixed native engine sends its original joined use set.
pub struct RootStorageActorDrain { instance: uuid::Uuid, slot: uuid::Uuid, retirement:uuid::Uuid,
    helper_readers: BTreeSet<uuid::Uuid> }
pub struct RootStorageRetired { _instance: uuid::Uuid, _slot: uuid::Uuid }

/// Root keeps this actual owner outside callbacks/catch_unwind/result delivery.
/// Every failed/unknown effect remains in its original slot. Drop never unmounts.
pub struct RootStorageService {
    endpoint: Endpoint, engine: Process, own: Process, engine_image: Image, service_image: Image,
    mnt: File, user: File, boot: Vec<u8>, instance: uuid::Uuid,
    root: Option<RootDirectory>, root_caps: u64, runtime:Option<crate::root_storage_bootstrap::runtime_image::RootRuntimeService>, paged:Option<paged::RootPagedStorage>,
    serial: u64, hello: bool, fenced: bool, slots: BTreeMap<uuid::Uuid, Slot>,
    retirement_answer:Option<RetirementAnswer>,outbox:Option<Vec<u8>>,pending_retired:Option<Slot>,
}
impl RootStorageService {
    /// Image bootstrap supplies only actual inherited endpoint and enrolled
    /// future engine pidfd; user-selected paths are never constructor inputs.
    pub fn from_bootstrap(endpoint: OwnedFd, original_engine_pidfd: OwnedFd) -> Result<Self> {
        ensure!(unsafe { libc::getuid() } == 0 && unsafe { libc::geteuid() } == 0
            && unsafe { libc::getgid() } == 0 && unsafe { libc::getegid() } == 0, "root storage requires actual UID/GID0");
        floor(std::process::id(), 0)?;
        let status = String::from_utf8(proc_bytes("/proc/thread-self/status", 16384)?)?;
        let caps = u64::from_str_radix(status.lines().find_map(|l| l.strip_prefix("CapEff:")).context("root storage caps absent")?.trim(), 16)?;
        ensure!(caps & (1 << 21) != 0 && caps & (1 << 19) != 0, "root storage requires actual SYS_ADMIN and SYS_PTRACE");
        let own = Process::current()?;
        let service_image = Image::capture(Path::new(SERVICE_IMAGE))?; service_image.matches_process(&own)?;
        let mut value = Self { endpoint: Endpoint::capture(endpoint)?, engine: Process::capture(original_engine_pidfd)?, own,
            engine_image: Image::capture(Path::new(ENGINE_IMAGE))?, service_image,
            mnt: namespace(None, "mnt", libc::CLONE_NEWNS)?, user: namespace(None, "user", libc::CLONE_NEWUSER)?,
            boot: proc_bytes("/proc/sys/kernel/random/boot_id", 64)?, instance: uuid::Uuid::new_v4(), serial: 0,
            root: None, root_caps: caps, runtime:None, paged:None, hello: false, fenced: false, slots: BTreeMap::new(),retirement_answer:None,outbox:None,pending_retired:None };
        ensure!(value.engine.pid != value.own.pid, "root storage cannot enroll itself as engine");
        ensure!(Path::new("/run").canonicalize()? == Path::new("/run"), "root storage /run redirects");
        let run = fs::symlink_metadata("/run")?;
        ensure!(run.is_dir() && run.uid() == 0 && run.gid() == 0 && run.mode() & 0o022 == 0, "root storage /run not root controlled");
        // No restart/saved-path adoption: an existing directory may contain a
        // previous process's unretired mounts/pins and requires explicit recovery.
        fs::DirBuilder::new().mode(0o700).create(ROOT_DIRECTORY)?;
        fs::set_permissions(ROOT_DIRECTORY, fs::Permissions::from_mode(0o700))?;
        value.root = Some(RootDirectory::capture(Path::new(ROOT_DIRECTORY))?);
        value.current()?; Ok(value)
    }
    fn current(&self) -> Result<()> {
        ensure!(!self.fenced && self.own.pid == std::process::id(), "root storage service fenced/process changed");
        ensure!(unsafe { libc::getuid() } == 0 && unsafe { libc::geteuid() } == 0
            && unsafe { libc::getgid() } == 0 && unsafe { libc::getegid() } == 0, "root storage current thread identity changed");
        floor(self.own.pid, 0)?; self.own.live()?; self.service_image.matches_process(&self.own)?;
        self.root.as_ref().context("storage original root directory absent")?.check()?;
        let status = String::from_utf8(proc_bytes("/proc/thread-self/status", 16384)?)?;
        ensure!(u64::from_str_radix(status.lines().find_map(|l| l.strip_prefix("CapEff:")).context("storage root caps absent")?.trim(), 16)? == self.root_caps,
            "storage root capabilities changed");
        ensure!(id(&self.mnt)? == id(&namespace(None, "mnt", libc::CLONE_NEWNS)?)?
            && id(&self.user)? == id(&namespace(None, "user", libc::CLONE_NEWUSER)?)?
            && self.boot == proc_bytes("/proc/sys/kernel/random/boot_id", 64)?, "root storage host namespace/boot changed"); Ok(())
    }
    fn authenticate(&self, credentials: &libc::ucred) -> Result<()> {
        self.current()?; self.engine.live()?;
        ensure!(credentials.pid == self.engine.pid as i32 && credentials.uid == 1500 && credentials.gid == 1500,
            "storage sender is not original enrolled engine");
        floor(self.engine.pid, 1500)?; self.engine_image.matches_process(&self.engine)?;
        ensure!(id(&self.mnt)? == id(&namespace(Some(self.engine.pid), "mnt", libc::CLONE_NEWNS)?)?
            && id(&self.user)? == id(&namespace(Some(self.engine.pid), "user", libc::CLONE_NEWUSER)?)?, "storage sender entered a guest namespace");
        self.engine.live()
    }
    pub fn close_admission(&mut self) { if let Some(runtime)=&mut self.runtime{runtime.close_admission();} if let Some(paged)=&mut self.paged{paged.close_admission();} self.fenced = true; for slot in self.slots.values_mut() { slot.fence(); } }
    /// OFF App, one bounded packet. No error removes a slot or its actual owners.
    pub fn poll_once(&mut self) -> Result<bool> {
        self.current()?;
        if let Some(runtime)=&mut self.runtime{if runtime.poll_retaining()?{return Ok(true)}}
        if let Some(bytes)=&self.outbox {
            if !self.endpoint.send(bytes,None)? {return Ok(false);}
            self.outbox=None;return Ok(true);
        }
        let Some(mut packet)=self.endpoint.receive()? else {return Ok(false);};
        self.authenticate(&packet.credentials)?;
        let (envelope,payload):(Envelope,_)=decode(&packet.bytes)?;
        ensure!(envelope.version==1&&packet.fds.len()<=1,"storage invalid envelope/descriptors");
        let digest=Sha256::digest(&packet.bytes).to_vec();
        // Replay ONLY the exact original held retirement ACK. No path/status
        // lookup, new operation, new budget, or publication promotion occurs.
        if envelope.serial==self.serial {
            let old=self.retirement_answer.as_ref().context("storage replay rejected")?;
            self.outbox=Some(old.replay(envelope.serial,&packet.bytes,!packet.fds.is_empty())?);return Ok(true);
        }
        ensure!(envelope.serial==self.serial.checked_add(1).context("storage serial exhausted")?,"storage packet order rejected");
        self.serial=envelope.serial;
        let retirement=match &envelope.request {
            Request::RetireSeed {slot,retirement,..}|Request::ReleaseSeed {slot,retirement}=>Some((*slot,*retirement)),_=>None};
        let result=(||->Result<Reply>{
            let wire_budget=FinalizationBudget::from_constraint(&envelope.constraint)?;wire_budget.remaining()?;
            self.dispatch(envelope.request,payload,packet.fds.pop(),&envelope.constraint)
        })();
        let reply=match result {Ok(reply)=>reply,Err(_)=>match retirement {
            Some((slot,retirement))=>Reply::RetirementPending {slot,retirement},
            None=>{for slot in self.slots.values_mut(){slot.fence();}Reply::Rejected}
        }};
        let bytes=encode(&Answer {version:1,serial:self.serial,reply},&[])?;
        if retirement.is_some() {self.retirement_answer=Some(RetirementAnswer {serial:self.serial,digest,bytes:bytes.clone(),_retired:self.pending_retired.take()});}
        else {self.retirement_answer=None;}
        // Queue-full never drops the actual ACK or root retirement owner.
        self.outbox=Some(bytes);Ok(true)
    }
    fn dispatch(&mut self, request: Request, payload: &[u8], fd: Option<OwnedFd>, wire_constraint: &FinalizationConstraint) -> Result<Reply> {
        if let Request::Hello { constraint } = request {
            ensure!(!self.hello && payload.is_empty() && fd.is_none() && &constraint == wire_constraint, "storage duplicate/invalid hello");
            FinalizationBudget::from_constraint(&constraint)?.remaining()?;
            self.hello = true; return Ok(Reply::Hello { instance: self.instance, image_sha256: self.service_image.sha256.clone() });
        }
        ensure!(self.hello, "storage hello required");
        let paged_slot=match &request {Request::Object{slot,..}|Request::PagedInventory{slot,..}|Request::PagedChunk{slot,..}|Request::Chunk{slot,..}|Request::Commit{slot}|Request::Check{slot}|Request::Fence{slot}
            |Request::EnrollHelper{slot}|Request::EnrollHelperCow{slot,..}|Request::RetireSeed{slot,..}|Request::ReleaseSeed{slot,..}|Request::CreateMetadataCow{slot,..}
            |Request::ArchiveMetadataCow{slot,..}=>self.paged.as_ref().is_some_and(|p|p.contains(slot)),_=>false};
        if matches!(&request,Request::BeginPaged{..})||paged_slot {
            if matches!(&request,Request::BeginPaged{..}) {
                ensure!(self.slots.len()+self.paged.as_ref().map_or(0,|p|p.slots())<MAX_SEEDS,"combined Root seed capacity full");
                if self.paged.is_none(){paged::RootPagedStorage::capture_retaining(&mut self.paged)?;}
            }
            return self.paged.as_mut().context("original paged Root owner absent")?.dispatch(request,payload,fd,wire_constraint,&self.engine,&self.engine_image);
        }
        if let Request::Begin { constraint, seed, refs, bytes } = request {
            ensure!(&constraint == wire_constraint && payload.is_empty() && self.slots.len()+self.paged.as_ref().map_or(0,|p|p.slots()) < MAX_SEEDS
                && self.slots.values().all(|s| s.published || s.fenced), "storage seed/preparation admission full");
            admit(&seed, refs, bytes)?;
            let budget = FinalizationBudget::from_constraint(&constraint)?;
            let cancel = Cancellation::capture(fd.context("storage original cancellation event required")?)?;
            let slot_id = uuid::Uuid::new_v4();
            ensure!(!self.slots.contains_key(&slot_id), "storage original slot nonce collision");
            self.slots.insert(slot_id, Slot { directory: Path::new(ROOT_DIRECTORY).join(slot_id.to_string()), directory_owner: None, seed, refs, bytes,
                received_bytes: 0, received: BTreeSet::new(), pending: None, store: None, stage: None,
                import_pin: format!("root-storage-import:{slot_id}"), pin_attempted: false, import_unpin_attempted: false, mount: None, reader: None,
                helpers: BTreeMap::new(), budget, cancel, fenced: false, published: false,retirement:None });
            let slot = self.slots.get_mut(&slot_id).unwrap();
            slot.cancel.start(slot.budget.clone())?; slot.current()?;
            fs::DirBuilder::new().mode(0o700).create(&slot.directory)?;
            slot.directory_owner = Some(RootDirectory::capture(&slot.directory)?);
            slot.store = Some(LocalStore::open(slot.directory.join("cas"))?);
            slot.stage = Some(slot.store()?.begin_staging(Duration::from_secs(120))?.id);
            slot.current()?; return Ok(Reply::Begun { slot: slot_id });
        }
        if let Request::RetireSeed {slot,retirement,readers}=request {
            ensure!(payload.is_empty()&&fd.is_none()&&readers.len()<=MAX_HELPERS_PER_SEED,"invalid native retirement command");
            let mut set=BTreeSet::new();for reader in readers {
                let id=uuid::Uuid::parse_str(&reader)?;ensure!(id.get_version_num()==4&&id.to_string()==reader&&set.insert(id),"retirement reader set invalid");
            }
            let proof=RootStorageActorDrain {instance:self.instance,slot,retirement,helper_readers:set};
            let original=FinalizationBudget::from_constraint(wire_constraint)?;
            self.retire_after_actor_drain_retaining(&proof,&original)?;
            return Ok(Reply::SeedRetired {slot,retirement});
        }
        if let Request::ReleaseSeed {slot,retirement}=request {
            ensure!(payload.is_empty()&&fd.is_none(),"invalid original seed release");
            let held=self.slots.get(&slot).context("original retired slot absent")?;
            let done=held.retirement.as_ref().context("original retirement absent")?;
            ensure!(done.id==retirement&&done.original.constraint()==*wire_constraint&&done.committed&&done.directory_removed,
                "original root cleanup not committed");
            done.original.remaining()?;self.current()?;
            // Native sends this ONLY after genuine Root ACK + observed removal
            // of its own source pin. All root effects are already proven.
            ensure!(self.pending_retired.is_none(),"previous Root retirement reply not staged");
            self.pending_retired=self.slots.remove(&slot);
            // Keep the actual fully-cleaned owner through ACK construction,
            // failed delivery and exact packet replay. Only a later original
            // authenticated serial can replace this bounded past-ACK history.
            return Ok(Reply::SeedReleased {slot,retirement});
        }
        let slot_id = match &request { Request::Object { slot, .. } | Request::Chunk { slot, .. } | Request::Commit { slot }
            | Request::Check { slot } | Request::EnrollHelper { slot } | Request::Fence { slot } => *slot, _ => anyhow::bail!("storage request names no original compatible slot") };
        let slot = self.slots.get_mut(&slot_id).context("storage original slot absent")?;
        if let Request::Fence { .. } = request { ensure!(payload.is_empty() && fd.is_none(), "invalid storage fence"); slot.fence(); return Ok(Reply::Accepted); }
        if let Request::EnrollHelper { .. } = request {
            ensure!(payload.is_empty() && slot.published && !slot.fenced && slot.retirement.is_none() && slot.helpers.len() < MAX_HELPERS_PER_SEED,
                "storage helper reader enrollment unavailable/full");
            let helper = Process::capture(fd.context("actual original helper pidfd required")?)?;
            self.engine.live()?; uid_identity(helper.pid, 1500)?; self.engine_image.matches_process(&helper)?;
            let status = String::from_utf8(proc_bytes(&format!("/proc/{}/status", helper.pid), 16384)?)?;
            ensure!(status.lines().find_map(|l| l.strip_prefix("PPid:")).context("helper parent absent")?.trim().parse::<u32>()? == self.engine.pid,
                "storage helper is not original engine child");
            helper.live()?;
            // Never replace an older helper's actual pidfd, including after
            // numeric PID reuse. Each original process gets its own reader ID.
            let reader = uuid::Uuid::new_v4(); let pid = helper.pid;
            ensure!(!slot.helpers.contains_key(&reader), "storage original reader nonce collision");
            slot.helpers.insert(reader, helper); return Ok(Reply::HelperEnrolled { reader, pid });
        }
        ensure!(fd.is_none(), "storage unexpected descriptor");
        if let Request::Check { .. } = request {
            ensure!(payload.is_empty() && !slot.fenced, "storage published slot fenced");
            slot.reader.as_ref().context("storage actual reader absent")?.check_current()?; return Ok(Reply::Accepted);
        }
        ensure!(slot.budget.constraint() == *wire_constraint, "storage original preparation deadline changed");
        slot.current()?; ensure!(!slot.published, "storage original seed already published");
        match request {
            Request::Object { reference, .. } => {
                object_bound(&reference, slot.bytes - slot.received_bytes)?;
                ensure!(payload.is_empty() && slot.pending.is_none() && slot.received.len() < slot.refs
                    && !slot.received.contains(&reference),
                    "storage object transfer exceeds original admission");
                let bytes = Vec::with_capacity(reference.size_bytes.try_into()?);
                slot.pending = Some((reference, bytes));
            }
            Request::Chunk { offset, .. } => {
                let (reference, bytes) = slot.pending.as_mut().context("storage original object not started")?;
                ensure!(offset == bytes.len() && payload.len() <= CHUNK && bytes.len() + payload.len() <= reference.size_bytes as usize
                    && (!payload.is_empty() || reference.size_bytes == 0), "storage chunk order/bound invalid");
                bytes.extend_from_slice(payload);
                if bytes.len() as u64 == reference.size_bytes {
                    let (reference, bytes) = slot.pending.as_ref().unwrap();
                    slot.store()?.receive_staged(slot.stage.as_deref().context("storage original staging absent")?, reference, bytes)?;
                    let (reference, _) = slot.pending.take().unwrap();
                    slot.received_bytes += reference.size_bytes; slot.received.insert(reference);
                }
            }
            Request::Commit { .. } => {
                ensure!(payload.is_empty() && slot.pending.is_none() && slot.received.len() == slot.refs && slot.received_bytes == slot.bytes
                    && slot.received.contains(&slot.seed), "storage original closure transfer incomplete");
                slot.pin_attempted = true;
                slot.store()?.publish(slot.stage.as_deref().context("storage original staging absent")?, &slot.import_pin, &slot.seed)?;
                slot.current()?;
                ensure!(slot.store()?.retained_closure(&slot.seed)?.into_iter().collect::<BTreeSet<_>>() == slot.received, "storage received refs differ from exact seed closure");
                slot.mount = Some(RootSeedMount::capture(slot.store()?.clone(), slot.seed.clone())?);
                slot.mount.as_mut().unwrap().prepare(&slot.budget, &slot.cancel.flag)?;
                slot.current()?;
                slot.reader = Some(slot.mount.as_mut().unwrap().acquire_read()?);
                slot.cancel.finish()?; slot.current()?;
                slot.published = true;
                let reader = slot.reader.as_ref().unwrap();
                return Ok(Reply::Ready { slot: slot_id, seed: slot.seed.clone(), root: reader.root().into(), objects: reader.objects() });
            }
            _ => anyhow::bail!("storage operation invalid for original slot"),
        }
        slot.current()?; Ok(Reply::Accepted)
    }
    /// Private authenticated engine command is the cross-process issuer. Root
    /// independently verifies original pidfds and exact full reader set. Rust
    /// join ownership never comes from a serialized boolean/receipt/path/TTL.
    pub fn retire_after_actor_drain_retaining(&mut self,proof:&RootStorageActorDrain,budget:&FinalizationBudget)->Result<RootStorageRetired> {
        self.current()?;budget.remaining()?;
        ensure!(proof.instance==self.instance,"retirement belongs to another original service");
        let slot=self.slots.get_mut(&proof.slot).context("original retirement slot absent")?;
        ensure!(!slot.fenced&&slot.published&&slot.pin_attempted,"unknown/unpublished seed cannot retire");
        ensure!(slot.helpers.keys().copied().collect::<BTreeSet<_>>()==proof.helper_readers,"full original helper-reader set differs");
        ensure!(slot.helpers.values().try_fold(true,|all,helper|Ok::<_,anyhow::Error>(all&&helper.exited()?))?,"original sibling helper still live");
        if let Some(old)=&slot.retirement {
            ensure!(old.id==proof.retirement&&old.readers==proof.helper_readers&&old.original.constraint()==budget.constraint(),"retirement intent/budget changed");
        } else {
            slot.mount.as_ref().context("original mount absent")?.close_admission();
            slot.retirement=Some(RootRetirement {id:proof.retirement,original:budget.clone(),readers:proof.helper_readers.clone(),
                import_removed:false,directory_remove_attempted:false,directory_removed:false,committed:false});
        }
        let original=slot.retirement.as_ref().unwrap().original.clone();original.remaining()?;
        if slot.retirement.as_ref().unwrap().committed {return Ok(RootStorageRetired {_instance:self.instance,_slot:proof.slot});}
        // Real native proof and all original kernel exits precede read-owner
        // release; an error never removes the Slot or substitutes a clock.
        slot.reader.take();
        slot.mount.as_mut().context("original mount absent")?.cleanup_after_reader_drain(&original)?;
        ensure!(slot.mount.as_ref().unwrap().is_settled(),"original mount cleanup uncertain");original.remaining()?;
        if !slot.retirement.as_ref().unwrap().import_removed {
            let store=slot.store()?.clone();
            match store.retention()?.pins.get(&slot.import_pin) {
                Some(root)=>{ensure!(root==&slot.seed,"original import pin changed");slot.import_unpin_attempted=true;store.unpin(&slot.import_pin)?;}
                None=>ensure!(slot.import_unpin_attempted,"import pin disappeared without original cleanup"),
            }
            ensure!(!store.retention()?.pins.contains_key(&slot.import_pin),"import unpin effect uncertain");
            slot.retirement.as_mut().unwrap().import_removed=true;
        }
        original.remaining()?;
        if !slot.retirement.as_ref().unwrap().directory_removed {
            match fs::symlink_metadata(&slot.directory) {
                Ok(_)=>{
                    slot.directory_owner.as_ref().context("original root directory absent")?.check()?;
                    ensure!(slot.store()?.retention()?.pins.is_empty(),"root seed CAS retains unexpected pins");original.remaining()?;
                    slot.retirement.as_mut().unwrap().directory_remove_attempted=true;
                    fs::remove_dir_all(&slot.directory)?;
                }
                Err(error) if error.kind()==std::io::ErrorKind::NotFound=>ensure!(slot.retirement.as_ref().unwrap().directory_remove_attempted,"root directory disappeared before cleanup intent"),
                Err(error)=>return Err(error.into()),
            }
            ensure!(matches!(fs::symlink_metadata(&slot.directory),Err(error) if error.kind()==std::io::ErrorKind::NotFound),"root directory absence unconfirmed");
            self.root.as_ref().context("original root parent absent")?.file.sync_all()?;
            slot.retirement.as_mut().unwrap().directory_removed=true;
        }
        original.remaining()?;slot.retirement.as_mut().unwrap().committed=true;
        Ok(RootStorageRetired {_instance:self.instance,_slot:proof.slot})
    }

}

/// Root-only fixed FD entry. The machine bootstrap interface that creates and
/// enrolls these descriptors is a separate, currently missing image change.
pub fn run_inherited_root_service() -> Result<()> {
    static TAKEN: AtomicBool = AtomicBool::new(false);
    ensure!(!TAKEN.swap(true, Ordering::AcqRel), "root storage inherited bootstrap already consumed");
    // FIRST descriptor operation: no constructor/proc/image/namespace/file
    // access may allocate into optional slots before their ownership snapshot.
    let mut inherited=unsafe{crate::root_storage_bootstrap::runtime_image::CapturedRuntimeDescriptors::take_at_service_entry()};
    let legacy_absent=inherited.is_absent_at_entry();
    let mut originals:[Option<OwnedFd>;2]=[None,None];
    let mut owner:Option<RootStorageService>=None;
    let setup=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||->Result<()> {
        // Retain actual core inputs outside constructor/unwind as well. Their
        // duplicates may allocate ONLY after the complete optional snapshot.
        let mut core_fault=false;
        for(index,raw)in [3,4].into_iter().enumerate(){let flags=unsafe{libc::fcntl(raw,libc::F_GETFD)};
            if flags<0{core_fault=true;continue}
            originals[index]=Some(unsafe{OwnedFd::from_raw_fd(raw)});
            if unsafe{libc::fcntl(raw,libc::F_SETFD,flags|libc::FD_CLOEXEC)}!=0{core_fault=true;}}
        inherited.validate_shape()?;ensure!(!core_fault,"root storage original bootstrap descriptor absent/unavailable");
        ensure!(std::env::args_os().count()==1&&std::env::vars_os().next().is_none(),"root storage needs fixed image bootstrap environment");
        owner=Some(RootStorageService::from_bootstrap(originals[0].as_ref().unwrap().try_clone()?,originals[1].as_ref().unwrap().try_clone()?)?);
        let actual=owner.as_mut().unwrap();
        crate::root_storage_bootstrap::runtime_image::RootRuntimeService::capture_entry_snapshot_retaining(&mut actual.runtime,&mut inherited,&actual.engine)
    }));
    let (setup_complete,retained_panic)=match setup {
        Ok(Ok(()))=>(true,None),
        Ok(Err(error)) if legacy_absent=>return Err(error),
        Err(payload) if legacy_absent=>std::panic::resume_unwind(payload),
        Ok(Err(_))=>(false,None),
        Err(payload)=>(false,Some(payload)),
    };
    if !setup_complete {
        if let Some(owner)=&mut owner{owner.close_admission();}
        loop {std::hint::black_box((&inherited,&originals,&owner,&retained_panic));std::thread::sleep(Duration::from_secs(1));}
    }
    let mut owner=owner.unwrap();
    loop {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.poll_once())) {
            Ok(Ok(true)) => {}, Ok(Ok(false)) => std::thread::sleep(Duration::from_millis(2)),
            Ok(Err(_)) | Err(_) => {
                owner.close_admission();
                // Preserve the actual carrier after disconnect/unknown effects.
                // Supervisor/VM shutdown owns recovery; no restart or TTL cleanup.
                loop { std::thread::sleep(Duration::from_secs(1)); }
            }
        }
    }
}

#[cfg(test)]
#[path = "root_storage_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "root_storage_ipc_qualification_tests.rs"]
mod ipc_qualification_tests;

#[cfg(test)]
#[path="root_storage_retirement_tests.rs"]
mod retirement_tests;

#[path="root_storage_paged.rs"]
pub(crate) mod paged;

#[cfg(test)]
#[path="root_storage_paged_tests.rs"]
mod paged_tests;
