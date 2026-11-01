//! Root-only small-seed producer in the CURRENT inherited mount namespace.
//! All methods, final FD disposal and normal unmount run off App. No namespace
//! entry, daemon, IPC, scheduling or backend startup is implemented here.
//!
//! Keep this actual mutable carrier OUTSIDE catch_unwind. prepare/cleanup borrow
//! it; every error retains its pin/mount/path intent. Drop never unmounts or
//! unpins. The supervisor must retain the carrier for explicit recovery.
#![cfg(target_os = "linux")]

use anyhow::{ensure, Context, Result};
use rudder_engine::{content, directory::EntryNode,
    execution::{finalization_budget::FinalizationBudget, network::linux::{NamespaceFd, NamespaceKind}},
    protocol::{canonical_json, ObjectKind, ObjectRef}, store::LocalStore, tree};
use rudder_vcs_bridge::seed::{MetadataTemplate, SeedFile, VerifiedMetadataTemplate};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, ffi::CString, fs::{self, File, OpenOptions},
    io::{Read, Write}, os::{fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt}}},
    path::{Component, Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, Ordering}}};

const PREFIX: &str = "/run/rudder-managed-seeds";
pub const ROOT_SEED_TMPFS_BYTES: u64 = 16 * 1024 * 1024;
pub const ROOT_SEED_MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FILES: usize = 1024;
const MAX_ENTRIES: usize = 4096;
const MAX_MANIFEST: u64 = 256 * 1024;
const MAX_READ_OWNERS: usize = 256;
const READER_UID: u32 = 1500;
const READER_GID: u32 = 1500;
const MOUNT_FLAGS: libc::c_ulong = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
const MOUNT_DATA: &std::ffi::CStr = c"size=16777216,nr_inodes=4096,mode=0700,uid=0,gid=0";

fn open_dir(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW
        | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?)
}
fn identity(file: &File) -> Result<(u64, u64)> {
    let m = file.metadata()?; Ok((m.dev(), m.ino()))
}
struct HeldDir { path: PathBuf, file: File, id: (u64, u64), mode: u32 }
impl HeldDir {
    fn root(path: &Path, mode: u32) -> Result<Self> {
        let file = open_dir(path)?; let id = identity(&file)?;
        let value = Self { path: path.into(), file, id, mode }; value.check()?; Ok(value)
    }
    fn check(&self) -> Result<()> {
        let named = fs::symlink_metadata(&self.path)?; let held = self.file.metadata()?;
        for m in [&named, &held] {
            ensure!(m.is_dir() && !m.file_type().is_symlink() && (m.dev(), m.ino()) == self.id
                && m.uid() == 0 && m.gid() == 0 && m.mode() & 0o7777 == self.mode,
                "root seed directory custody changed");
        }
        Ok(())
    }
    fn mode(&mut self, mode: u32) -> Result<()> {
        self.check()?; self.file.set_permissions(fs::Permissions::from_mode(mode))?;
        self.mode = mode; self.check()
    }
}
fn root_ancestors(path: &Path) -> Result<()> {
    ensure!(path.is_absolute() && path.canonicalize()? == path, "root seed path is not canonical");
    for path in path.ancestors() {
        let m = fs::symlink_metadata(path)?;
        ensure!(m.is_dir() && !m.file_type().is_symlink() && m.uid() == 0 && m.gid() == 0
            && m.mode() & 0o022 == 0, "root seed ancestor is not root controlled");
    }
    Ok(())
}
fn proc_bytes(path: &str, maximum: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC
        | libc::O_NONBLOCK).open(path)?;
    let mut kind: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(unsafe { libc::fstatfs(file.as_raw_fd(), &mut kind) } == 0 && kind.f_type == 0x9fa0,
        "root seed procfs unavailable");
    let mut bytes = Vec::new(); file.take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= maximum, "root seed procfs bound exceeded"); Ok(bytes)
}
fn mount_namespace() -> Result<File> {
    // Intentional kernel namespace magic link; no caller-supplied pathname.
    let file = OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC)
        .open("/proc/thread-self/ns/mnt")?;
    let mut kind: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(unsafe { libc::fstatfs(file.as_raw_fd(), &mut kind) } == 0 && kind.f_type == 0x6e736673
        && unsafe { libc::ioctl(file.as_raw_fd(), libc::NS_GET_NSTYPE as libc::c_ulong) } == libc::CLONE_NEWNS,
        "root seed mount namespace is not nsfs/MNT"); Ok(file)
}
fn user_namespace() -> Result<NamespaceFd> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC)
        .open("/proc/thread-self/ns/user")?;
    Ok(NamespaceFd::inspect(file.into(), NamespaceKind::User)?)
}
struct RootCustody { process: u32, pidfd: OwnedFd, mnt: File, user: NamespaceFd,
    boot: File, boot_id: (u64, u64), boot_bytes: Vec<u8> }
impl RootCustody {
    fn capture() -> Result<Self> {
        let process = std::process::id();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, process, 0) } as i32;
        ensure!(raw >= 0, "root seed pidfd unavailable: {}", std::io::Error::last_os_error());
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw) };
        let flags = unsafe { libc::fcntl(pidfd.as_raw_fd(), libc::F_GETFD) };
        ensure!(flags >= 0 && flags & libc::FD_CLOEXEC != 0,
            "root seed pidfd is inheritable");
        let boot = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/proc/sys/kernel/random/boot_id")?;
        let boot_id = identity(&boot)?; let boot_bytes = read_boot(&boot)?;
        let value = Self { process, pidfd, mnt: mount_namespace()?, user: user_namespace()?,
            boot, boot_id, boot_bytes }; value.check()?; Ok(value)
    }
    fn check(&self) -> Result<()> {
        ensure!(unsafe { libc::getuid() } == 0 && unsafe { libc::geteuid() } == 0
            && unsafe { libc::getgid() } == 0 && unsafe { libc::getegid() } == 0,
            "root seed producer requires real/effective UID/GID0");
        let status = String::from_utf8(proc_bytes("/proc/thread-self/status", 16384)?)?;
        let caps = status.lines().find_map(|line| line.strip_prefix("CapEff:")).context("root capability set absent")?.trim();
        ensure!(caps.len() == 16 && u64::from_str_radix(caps, 16)? & (1 << 21) != 0,
            "root seed CAP_SYS_ADMIN absent");
        let mut poll = libc::pollfd { fd: self.pidfd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        ensure!(self.process == std::process::id() && unsafe { libc::poll(&mut poll, 1, 0) } == 0,
            "root seed process custody changed");
        ensure!(identity(&self.mnt)? == identity(&mount_namespace()?)?
            && self.user.identity() == user_namespace()?.identity(), "root seed namespace changed");
        let named = fs::symlink_metadata("/proc/sys/kernel/random/boot_id")?;
        ensure!(named.is_file() && !named.file_type().is_symlink()
            && (named.dev(), named.ino()) == self.boot_id && identity(&self.boot)? == self.boot_id
            && read_boot(&self.boot)? == self.boot_bytes, "root seed boot custody changed"); Ok(())
    }
}
fn read_boot(file: &File) -> Result<Vec<u8>> {
    let mut kind: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(unsafe { libc::fstatfs(file.as_raw_fd(), &mut kind) } == 0 && kind.f_type == 0x9fa0,
        "root seed boot descriptor is not procfs");
    let mut bytes = [0; 65]; let n = file.read_at(&mut bytes, 0)?;
    ensure!(n == 37 && bytes[36] == b'\n' && uuid::Uuid::parse_str(std::str::from_utf8(&bytes[..36])?)?
        .to_string().as_bytes() == &bytes[..36], "root seed boot ID malformed"); Ok(bytes[..n].to_vec())
}

/// Only the original held inode and a pre-effect intent permit absent-path
/// reconciliation. A replacement path (including a symlink) is never cleanup.
fn original_directory_present(held:&HeldDir,attempted:bool)->Result<bool> {
    match fs::symlink_metadata(&held.path) {
        Ok(_)=>{held.check()?;Ok(true)}
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{
            ensure!(attempted,"owned directory disappeared before original remove intent");
            let m=held.file.metadata()?;
            ensure!((m.dev(),m.ino())==held.id&&m.is_dir()&&m.uid()==0&&m.gid()==0
                &&m.mode()&0o7777==held.mode,"original removed directory custody changed");Ok(false)
        }
        Err(error)=>Err(error.into()),
    }
}
fn remove_original_directory(held:&HeldDir,attempted:&mut bool,original:&FinalizationBudget)->Result<()> {
    original.remaining()?;
    if original_directory_present(held,*attempted)? {
        *attempted=true;if let Err(error)=fs::remove_dir(&held.path) {*attempted=false;return Err(error.into());}
    }
    ensure!(!original_directory_present(held,*attempted)?,"original directory removal unconfirmed");
    original.remaining()?;Ok(())
}

struct SourcePin { store: LocalStore, directory: HeldDir, seed: ObjectRef, name: String,
    attempted: bool, acquired: bool, removed: bool, unpin_attempted:bool }
impl SourcePin {
    fn capture(store: LocalStore, seed: ObjectRef, nonce: uuid::Uuid) -> Result<Self> {
        seed.validate()?; ensure!(seed.kind == ObjectKind::Directory, "root seed must be a Directory");
        // Pin/unpin writes must NEVER target the UID1500 engine's store.
        root_ancestors(store.root())?; let directory = HeldDir::root(store.root(), 0o700)?;
        Ok(Self { store, directory, seed, name: format!("root-seed-mount:{nonce}"),
            attempted: false, acquired: false, removed: false,unpin_attempted:false })
    }
    fn acquire(&mut self) -> Result<()> {
        self.directory.check()?;
        ensure!(!self.attempted && !self.store.retention()?.pins.contains_key(&self.name), "root seed pin already attempted");
        self.attempted = true; // BEFORE a possibly effect-unknown store write.
        self.store.pin_existing(&self.name, &self.seed)?; self.acquired = true; self.check()
    }
    fn check(&self) -> Result<()> {
        self.directory.check()?;
        ensure!(self.acquired && !self.removed
            && self.store.retention()?.pins.get(&self.name) == Some(&self.seed), "root seed pin custody changed");
        self.directory.check()
    }
    fn release(&mut self) -> Result<()> {
        self.directory.check()?;
        if !self.attempted { self.removed = true; return Ok(()); }
        match self.store.retention()?.pins.get(&self.name) {
            Some(root) => { ensure!(root == &self.seed, "root seed pin was replaced"); self.unpin_attempted=true;self.store.unpin(&self.name)?; }
            None => ensure!(!self.acquired||self.unpin_attempted,"acquired root pin vanished before original unpin intent"),
        }
        ensure!(!self.store.retention()?.pins.contains_key(&self.name), "root seed pin removal unconfirmed");
        self.directory.check()?; self.removed = true; Ok(())
    }
}

struct PlannedFile { path: PathBuf, object: ObjectRef, mode: u32, descriptor: SeedFile }
struct Plan { directories: Vec<(PathBuf, u32)>, files: Vec<PlannedFile>, bytes: u64 }
fn relative(path: &str) -> Result<PathBuf> {
    let value = Path::new(path);
    ensure!(!path.is_empty() && path.len() <= 512 && value.components().count() <= 32
        && value.components().all(|c| matches!(c, Component::Normal(_)))
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b)), "root seed path exceeds small profile");
    Ok(value.into())
}
fn plan(store: &LocalStore, seed: &ObjectRef, checkpoint: &impl Fn() -> Result<()>) -> Result<Plan> {
    checkpoint()?;
    // tree::lookup can decode a legacy Directory before reaching the later
    // traversal guards; refuse the initial ref BEFORE any such CAS read.
    ensure!(seed.kind == ObjectKind::Directory && seed.size_bytes <= MAX_MANIFEST,
        "initial root seed directory exceeds small profile");
    let entry = tree::lookup(store, seed, b"manifest.json")?.context("root seed manifest absent")?;
    let EntryNode::File { object, logical_size, .. } = entry.node else { anyhow::bail!("root seed manifest is not a file") };
    ensure!(logical_size > 0 && logical_size <= MAX_MANIFEST, "root seed manifest exceeds small profile");
    let bytes = content::read_file_range(store, &object, 0, logical_size.try_into()?)?;
    let manifest: MetadataTemplate = serde_json::from_slice(&bytes)?; manifest.validate()?;
    ensure!(canonical_json(&manifest)? == bytes, "root seed manifest is not canonical");
    let mut expected: BTreeMap<PathBuf, SeedFile> = BTreeMap::new();
    for (prefix, records) in [("metadata", &manifest.metadata), ("objects", &manifest.objects)] {
        for (name, record) in records { expected.insert(Path::new(prefix).join(relative(name)?), record.clone()); }
    }
    expected.insert("manifest.json".into(), SeedFile { sha256: format!("{:x}", Sha256::digest(&bytes)), size_bytes: logical_size });
    ensure!(expected.len() <= MAX_FILES, "root seed has too many files");
    let mut plan = Plan { directories: Vec::new(), files: Vec::new(), bytes: 0 };
    let mut pending = vec![(PathBuf::new(), seed.clone())]; let mut entries = 0; let mut allocated = 0u64;
    while let Some((path, directory)) = pending.pop() {
        checkpoint()?;
        ensure!(directory.kind == ObjectKind::Directory && directory.size_bytes <= MAX_MANIFEST, "root seed directory exceeds small profile");
        let info = tree::root_info(store, &directory)?;
        ensure!(info.mode & !0o777 == 0 && info.mode & 0o500 == 0o500, "root seed directory mode unsupported");
        plan.directories.push((path.clone(), info.mode)); allocated = allocated.checked_add(4096).context("seed allocation overflow")?;
        let mut after = None;
        loop {
            checkpoint()?; let page = tree::scan(store, &directory, after.as_deref(), 128)?;
            for entry in page.entries {
                entries += 1; ensure!(entries <= MAX_ENTRIES, "root seed namespace exceeds bound");
                let name = String::from_utf8(entry.name()?)?; let rel = path.join(relative(&name)?);
                relative(rel.to_str().context("seed path is not UTF8")?)?;
                match entry.node {
                    EntryNode::Directory { object } => {
                        ensure!(rel.starts_with("metadata") || rel.starts_with("objects"), "unexpected root seed directory");
                        pending.push((rel, object));
                    }
                    EntryNode::File { object, logical_size, mode } => {
                        let descriptor = expected.remove(&rel).context("unexpected root seed file")?;
                        ensure!(logical_size == descriptor.size_bytes && mode & !0o777 == 0 && mode & 0o400 != 0,
                            "root seed size/mode differs from manifest profile");
                        plan.bytes = plan.bytes.checked_add(logical_size).context("seed byte overflow")?;
                        ensure!(plan.bytes <= ROOT_SEED_MAX_BYTES, "root seed bytes exceed small profile");
                        allocated = allocated.checked_add(logical_size.div_ceil(4096) * 4096).context("seed allocation overflow")?;
                        plan.files.push(PlannedFile { path: rel, object, mode, descriptor });
                    }
                    EntryNode::Symlink { .. } => anyhow::bail!("symlink in root seed"),
                }
            }
            after = page.next_after; if after.is_none() { break; }
        }
    }
    ensure!(expected.is_empty() && plan.directories.iter().any(|(p,_)| p == Path::new("metadata"))
        && plan.directories.iter().any(|(p,_)| p == Path::new("objects")), "root seed namespace incomplete");
    ensure!(allocated <= 12 * 1024 * 1024, "root seed page/inode allocation exceeds small profile");
    plan.directories.sort_by_key(|(p,_)| p.components().count()); checkpoint()?; Ok(plan)
}

#[derive(Clone, Debug)]
struct Mount { id: u64, filesystem: String, source: String, options: Vec<String>, super_options: Vec<String> }
fn mount_at(path: &Path) -> Result<Option<Mount>> {
    let path = path.to_str().context("seed mount is not UTF8")?;
    let bytes = proc_bytes("/proc/thread-self/mountinfo", 1024 * 1024)?;
    let mut found = None;
    for line in std::str::from_utf8(&bytes)?.lines() {
        let (left, right) = line.split_once(" - ").context("incomplete mountinfo line")?;
        let left: Vec<_> = left.split(' ').collect(); let right: Vec<_> = right.split(' ').collect();
        ensure!(left.len() >= 6 && right.len() >= 3, "incomplete mountinfo fields");
        if left[4] != path { continue; }
        ensure!(found.is_none(), "ambiguous root seed mount");
        let id: u64 = left[0].parse()?; ensure!(id > 0, "invalid root seed mount ID");
        found = Some(Mount { id, filesystem: right[0].into(), source: right[1].into(),
            options: left[5].split(',').map(str::to_owned).collect(), super_options: right[2].split(',').map(str::to_owned).collect() });
    }
    Ok(found)
}
fn syscall(value: i32, label: &'static str) -> Result<()> {
    if value != 0 { return Err(std::io::Error::last_os_error()).context(label); } Ok(())
}
fn checkpoint(budget: &FinalizationBudget, cancelled: &AtomicBool) -> Result<()> {
    ensure!(!cancelled.load(Ordering::Acquire), "root seed preparation cancelled"); budget.remaining()?; Ok(())
}
fn validate_with_original_budget(budget: &FinalizationBudget, cancelled: &AtomicBool,
    validate: impl FnOnce() -> Result<()>) -> Result<()> {
    checkpoint(budget, cancelled)?;
    validate()?; // Includes actual CAS retention locks; return may be late.
    checkpoint(budget, cancelled)
}

struct Resources {
    custody: RootCustody, source: SourcePin, session: PathBuf, target: PathBuf, label: CString,
    prefix: Option<HeldDir>, session_dir: Option<HeldDir>, underlying: Option<HeldDir>, mounted: Option<File>,
    session_created: bool, target_created: bool, mount_attempted: bool, mount_id: Option<u64>,
    unmounted: bool, prepare_attempted: bool, template: Option<VerifiedMetadataTemplate>, bytes: u64,
    retiring: AtomicBool, fenced: AtomicBool, cleaned: bool,
    unmount_attempted:bool,target_remove_attempted:bool,session_remove_attempted:bool,cleanup_budget:Option<FinalizationBudget>,
}
impl Resources {
    fn paths_current(&self) -> Result<()> {
        self.custody.check()?; self.source.directory.check()?;
        if let Some(prefix) = &self.prefix { prefix.check()?; }
        if let Some(session) = &self.session_dir { session.check()?; } Ok(())
    }
    fn current_mount(&self, readonly: bool) -> Result<Mount> {
        self.paths_current()?;
        let m = mount_at(&self.target)?.context("owned root seed mount absent")?;
        ensure!(m.filesystem == "tmpfs" && m.source.as_bytes() == self.label.as_bytes()
            && self.mount_id.is_none_or(|id| id == m.id), "root seed mount was replaced");
        for flag in ["nosuid", "nodev", "noexec"] { ensure!(m.options.iter().any(|s| s == flag), "root seed safety flag lost"); }
        if readonly { ensure!(m.options.iter().any(|s| s == "ro") && m.super_options.iter().any(|s| s == "ro"), "root seed lost readonly seal"); }
        if let Some(held) = &self.mounted {
            let named = open_dir(&self.target)?; ensure!(identity(held)? == identity(&named)?, "root seed mount FD differs");
            let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
            ensure!(unsafe { libc::fstatfs(held.as_raw_fd(), &mut stat) } == 0 && stat.f_type == 0x01021994
                && (stat.f_bsize as u64).checked_mul(stat.f_blocks as u64).is_some_and(|n| n > 0 && n <= ROOT_SEED_TMPFS_BYTES), "root seed tmpfs capacity/type changed");
        }
        Ok(m)
    }
    fn check(&self) -> Result<()> {
        let result = (|| {
            ensure!(!self.fenced.load(Ordering::Acquire) && !self.cleaned && self.template.is_some(), "root seed unavailable/fenced");
            self.source.check()?; self.current_mount(true)?;
            ensure!(!self.fenced.load(Ordering::Acquire), "root seed fenced during validation"); Ok(())
        })();
        if result.is_err() { self.fenced.store(true, Ordering::Release); } result
    }
}

/// Original root-process carrier. No Serde/path/boolean constructor, no Clone.
/// Its read owners are lifetime custody, NOT a fabricated helper-drain ACK.
#[must_use = "retain through actual reader drain and explicit normal cleanup"]
pub struct RootSeedMount { resources: Arc<Resources> }
#[must_use = "keep in the original helper/session carrier until actual reader drain"]
pub struct SeedMountRead { resources: Arc<Resources> }
impl RootSeedMount {
    /// Read-only capture. The supplied store must ALREADY be an actually pinned,
    /// root-owned CAS under root-controlled ancestors, not the engine UID1500 CAS.
    pub fn capture(store: LocalStore, seed: ObjectRef) -> Result<Self> {
        let custody = RootCustody::capture()?; let nonce = uuid::Uuid::new_v4();
        let source = SourcePin::capture(store, seed, nonce)?;
        let session = Path::new(PREFIX).join(nonce.to_string()); let target = session.join("seed");
        Ok(Self { resources: Arc::new(Resources { custody, source, session, target,
            label: CString::new(format!("rudder-seed-{nonce}"))?, prefix: None, session_dir: None,
            underlying: None, mounted: None, session_created: false, target_created: false,
            mount_attempted: false, mount_id: None, unmounted: false, prepare_attempted: false,
            unmount_attempted:false,target_remove_attempted:false,session_remove_attempted:false,cleanup_budget:None,
            template: None, bytes: 0, retiring: AtomicBool::new(false), fenced: AtomicBool::new(false), cleaned: false }) })
    }
    /// Original admission deadline/cancel must be captured before worker queueing.
    /// A blocked CAS lock/syscall retains this owner; expiry never proves settlement.
    pub fn prepare(&mut self, budget: &FinalizationBudget, cancelled: &AtomicBool) -> Result<()> {
        let r = Arc::get_mut(&mut self.resources).context("root seed has live read owners")?;
        ensure!(!r.prepare_attempted && !r.retiring.load(Ordering::Acquire), "root seed preparation already attempted");
        r.prepare_attempted = true;
        let result = (|| {
            checkpoint(budget, cancelled)?; r.paths_current()?; r.source.acquire()?;
            let plan = plan(&r.source.store, &r.source.seed, &|| { checkpoint(budget, cancelled)?; r.source.check() })?;
            root_ancestors(Path::new("/run"))?;
            match fs::DirBuilder::new().mode(0o711).create(PREFIX) {
                Ok(()) => { open_dir(Path::new(PREFIX))?.set_permissions(fs::Permissions::from_mode(0o711))?; },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}, Err(error) => return Err(error.into()),
            }
            r.prefix = Some(HeldDir::root(Path::new(PREFIX), 0o711)?);
            checkpoint(budget, cancelled)?; r.paths_current()?;
            fs::DirBuilder::new().mode(0o700).create(&r.session)?; r.session_created = true;
            r.session_dir = Some(HeldDir::root(&r.session, 0o700)?);
            fs::DirBuilder::new().mode(0o700).create(&r.target)?; r.target_created = true;
            r.underlying = Some(HeldDir::root(&r.target, 0o700)?);
            ensure!(mount_at(&r.target)?.is_none(), "root seed target already mounted");
            let target = CString::new(r.target.as_os_str().as_bytes())?;
            checkpoint(budget, cancelled)?; r.paths_current()?; r.mount_attempted = true;
            syscall(unsafe { libc::mount(r.label.as_ptr(), target.as_ptr(), c"tmpfs".as_ptr(), MOUNT_FLAGS, MOUNT_DATA.as_ptr().cast()) }, "mount fresh root seed tmpfs")?;
            // Record the actual mount before further allocation/validation.
            r.mounted = Some(open_dir(&r.target)?); r.mount_id = Some(r.current_mount(false)?.id);
            for (path, _) in plan.directories.iter().filter(|(p,_)| !p.as_os_str().is_empty()) {
                checkpoint(budget, cancelled)?; fs::DirBuilder::new().mode(0o700).create(r.target.join(path))?;
            }
            for entry in plan.files {
                checkpoint(budget, cancelled)?; r.source.check()?;
                let bytes = content::read_file_range(&r.source.store, &entry.object, 0, entry.descriptor.size_bytes.try_into()?)?;
                ensure!(bytes.len() as u64 == entry.descriptor.size_bytes
                    && format!("{:x}", Sha256::digest(&bytes)) == entry.descriptor.sha256, "root seed file checksum differs");
                checkpoint(budget, cancelled)?;
                let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(r.target.join(entry.path))?;
                file.write_all(&bytes)?;
                checkpoint(budget, cancelled)?;
                syscall(unsafe { libc::fchown(file.as_raw_fd(), READER_UID, READER_GID) }, "chown only fresh seed file")?;
                file.set_permissions(fs::Permissions::from_mode(entry.mode))?; file.sync_all()?;
            }
            // Only this fresh tmpfs tree is chowned; no source/CAS/existing file.
            // Root-owned session remains mode0700 until AFTER the RO seal.
            for (path, mode) in plan.directories.iter().rev() {
                checkpoint(budget, cancelled)?;
                let directory = open_dir(&r.target.join(path))?;
                syscall(unsafe { libc::fchown(directory.as_raw_fd(), READER_UID, READER_GID) }, "chown only fresh seed directory")?;
                directory.set_permissions(fs::Permissions::from_mode(*mode))?; directory.sync_all()?;
            }
            checkpoint(budget, cancelled)?; r.paths_current()?; r.source.check()?;
            syscall(unsafe { libc::mount(std::ptr::null(), target.as_ptr(), std::ptr::null(),
                libc::MS_REMOUNT | libc::MS_RDONLY | MOUNT_FLAGS, MOUNT_DATA.as_ptr().cast()) }, "seal root seed readonly")?;
            r.current_mount(true)?;
            let probe = OpenOptions::new().write(true).create_new(true).mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(r.target.join(".root-write-probe"));
            ensure!(probe.as_ref().err().and_then(|e| e.raw_os_error()) == Some(libc::EROFS), "root seed write did not fail EROFS");
            r.template = Some(VerifiedMetadataTemplate::open_retained(&r.source.store, &r.source.seed,
                &r.target, &r.target.join("objects"))?); r.bytes = plan.bytes;
            validate_with_original_budget(budget, cancelled, || r.check())?;
            // Opening traversal is the final effect; reader can never see RW.
            r.session_dir.as_mut().unwrap().mode(0o711)?;
            validate_with_original_budget(budget, cancelled, || r.check())
        })();
        if result.is_err() { r.fenced.store(true, Ordering::Release); } result
    }
    /// Root-side retained reader custody. Future IPC keeps this slot through
    /// actual original helper shutdown/exit/joins, including failed ACK delivery.
    /// Dropping it is NOT independently evidence that any process drained.
    pub fn acquire_read(&mut self) -> Result<SeedMountRead> {
        ensure!(!self.resources.retiring.load(Ordering::Acquire) && self.read_owners() < MAX_READ_OWNERS,
            "root seed read admission closed/full"); self.resources.check()?;
        ensure!(!self.resources.retiring.load(Ordering::Acquire), "root seed retiring");
        Ok(SeedMountRead { resources: Arc::clone(&self.resources) })
    }
    pub fn read_owners(&self) -> usize { Arc::strong_count(&self.resources).saturating_sub(1) }
    pub fn close_admission(&self) { self.resources.retiring.store(true, Ordering::Release); }
    pub fn is_settled(&self) -> bool { self.resources.cleaned }
    /// Explicit recovery after preparation failure OR verified physical reader
    /// drain. Lifetime count is an additional gate, not the physical drain proof.
    /// Normal umount only; EBUSY/unknown effects retain the carrier and source pin.
    pub fn cleanup_after_reader_drain(&mut self, budget: &FinalizationBudget) -> Result<()> {
        self.close_admission();
        let r = Arc::get_mut(&mut self.resources).context("root seed still has retained reader owners")?;
        if let Some(original)=&r.cleanup_budget {ensure!(original.constraint()==budget.constraint(),"root seed cleanup budget changed");}
        else {budget.remaining()?;r.cleanup_budget=Some(budget.clone());}
        if r.cleaned { return Ok(()); }
        let original=r.cleanup_budget.as_ref().unwrap().clone();original.remaining()?;
        // Normal read checks require named paths. Cleanup additionally permits
        // an absent directory ONLY after our retained original unlink intent.
        r.custody.check()?;r.source.directory.check()?;
        if let Some(prefix)=&r.prefix {prefix.check()?;}
        if let Some(session)=&r.session_dir {original_directory_present(session,r.session_remove_attempted)?;}
        r.template = None;
        if r.mount_attempted && !r.unmounted {
            if let Some(mount) = mount_at(&r.target)? {
                ensure!(mount.filesystem == "tmpfs" && mount.source.as_bytes() == r.label.as_bytes()
                    && r.mount_id.is_none_or(|id| id == mount.id), "cleanup refuses replaced root seed mount");
                original.remaining()?;r.custody.check()?;
                // Our own directory descriptor must not cause EBUSY. This
                // original intent, not path absence alone, permits recovery.
                drop(r.mounted.take()); let target = CString::new(r.target.as_os_str().as_bytes())?;
                r.unmount_attempted=true;
                let result = syscall(unsafe { libc::umount2(target.as_ptr(), 0) }, "normal root seed unmount");
                if let Err(error) = result { r.unmount_attempted=false;r.mounted = open_dir(&r.target).ok(); return Err(error); }
                r.unmounted = true;
            } else {
                ensure!(r.mount_id.is_none()||r.unmount_attempted,"owned root seed disappeared without original normal-unmount intent");
                // Held underlying inode must be exposed after actual unmount.
                if let Some(underlying)=&r.underlying {original_directory_present(underlying,r.target_remove_attempted)?;}
                r.unmounted=true;r.mounted=None;
            }
        }
        original.remaining()?;r.custody.check()?;
        ensure!(mount_at(&r.target)?.is_none(), "root seed mount absence unconfirmed");
        if r.target_created {
            remove_original_directory(r.underlying.as_ref().context("underlying root seed directory absent")?,
                &mut r.target_remove_attempted,&original)?;
            r.target_created=false;r.underlying=None;
        }
        if r.session_created {
            remove_original_directory(r.session_dir.as_ref().context("root seed session directory absent")?,
                &mut r.session_remove_attempted,&original)?;
            r.session_created=false;r.session_dir=None;
        }
        original.remaining()?;r.custody.check()?;r.source.release()?;original.remaining()?;
        r.cleaned = true; Ok(())
    }

}
impl SeedMountRead {
    pub fn check_current(&self) -> Result<()> { self.resources.check() }
    pub fn seed(&self) -> &ObjectRef { &self.resources.source.seed }
    pub fn root(&self) -> &Path { &self.resources.target }
    pub fn objects(&self) -> PathBuf { self.resources.target.join("objects") }
    pub fn template(&self) -> Result<&VerifiedMetadataTemplate> {
        self.check_current()?; self.resources.template.as_ref().context("root seed template absent")
    }
}

#[cfg(test)]
#[path = "root_seed_mount_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "root_seed_real_history_tests.rs"]
mod real_history_tests;

#[cfg(test)]
#[path="root_seed_retirement_cleanup_tests.rs"]
mod retirement_cleanup_tests;
