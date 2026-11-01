//! Trusted host-engine client only. No public CLI or path/JSON mount-adoption
//! API. Keep actual transfer/client owners outside worker/result callbacks.
#![cfg(target_os = "linux")]
use anyhow::{ensure, Context, Result};
use crate::root_storage_service::{self as service, Answer, Endpoint, Envelope, Image, Process, Reply, Request,
    CHUNK, ENGINE_IMAGE, MAX_HELPERS_PER_SEED, MAX_OBJECT_BYTES, MAX_REFS, MAX_TRANSFER_BYTES, SERVICE_IMAGE};
use rudder_engine::{execution::finalization_budget::FinalizationBudget,
    protocol::{ObjectKind, ObjectRef}, store::LocalStore};
use rudder_vcs_bridge::seed::VerifiedMetadataTemplate;
use std::{collections::{BTreeMap, BTreeSet}, fs::{self, File, OpenOptions}, os::{fd::{AsRawFd, FromRawFd, OwnedFd}, unix::fs::{MetadataExt, OpenOptionsExt}},
    path::{Path, PathBuf}, sync::atomic::{AtomicBool, Ordering}, time::Duration};
static INHERITED_BOOTSTRAP_TAKEN: AtomicBool = AtomicBool::new(false);

/// The source is already pinned by the caller. Check every newly discovered
/// typed reference BEFORE reading it or growing the retained transfer queue.
pub(crate) fn bounded_inventory(root: &ObjectRef, original: &FinalizationBudget,
    mut references: impl FnMut(&ObjectRef) -> Result<Vec<ObjectRef>>) -> Result<Vec<ObjectRef>> {
    service::object_bound(root, MAX_TRANSFER_BYTES)?;
    let mut known = BTreeSet::from([root.clone()]); let mut pending = vec![root.clone()]; let mut total = root.size_bytes;
    while let Some(reference) = pending.pop() {
        original.remaining()?;
        let children = references(&reference)?;
        original.remaining()?;
        for child in children {
            if known.contains(&child) { continue; }
            ensure!(known.len() < MAX_REFS, "storage source closure exceeds ref bound");
            service::object_bound(&child, MAX_TRANSFER_BYTES - total)?;
            total += child.size_bytes; known.insert(child.clone()); pending.push(child);
        }
    }
    ensure!(total > 0, "storage source closure is empty"); original.remaining()?;
    Ok(known.into_iter().collect())
}

struct SourcePin {
    store: LocalStore, directory: File, identity: (u64, u64), seed: ObjectRef, pin: String,
    attempted: bool, acquired: bool,
}
impl SourcePin {
    fn capture(store: LocalStore, seed: ObjectRef) -> Result<Self> {
        seed.validate()?; ensure!(seed.kind == ObjectKind::Directory && seed.size_bytes <= 256 * 1024, "storage client seed exceeds small profile");
        let directory = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(store.root())?;
        let pin = Self { identity: service::id(&directory)?, directory, store, seed,
            pin: format!("root-storage-client:{}", uuid::Uuid::new_v4()), attempted: false, acquired: false };
        pin.current_directory()?; Ok(pin)
    }
    fn current_directory(&self) -> Result<()> {
        for m in [self.directory.metadata()?, fs::symlink_metadata(self.store.root())?] {
            ensure!(m.is_dir() && !m.file_type().is_symlink() && (m.dev(), m.ino()) == self.identity
                && m.uid() == 1500 && m.gid() == 1500 && m.mode() & 0o7777 == 0o700, "storage client original UID1500 CAS changed");
        } Ok(())
    }
    fn acquire(&mut self) -> Result<()> {
        self.current_directory()?;
        ensure!(!self.attempted && !self.store.retention()?.pins.contains_key(&self.pin), "storage client pin already attempted/collided");
        self.attempted = true;
        self.store.pin_existing(&self.pin, &self.seed)?; self.acquired = true; self.check()
    }
    fn check(&self) -> Result<()> {
        self.current_directory()?;
        ensure!(self.acquired && self.store.retention()?.pins.get(&self.pin) == Some(&self.seed), "storage client source pin not retained"); Ok(())
    }
}

/// Original client-side transfer/read owner. Not Clone/Serde, and Drop never
/// unpins. Errors retain the actual source pin, cancel event and helper pidfd.
#[must_use = "retain through original helper and actor drain plus root retirement acknowledgement"]
pub struct ClientSeedOwner {
    source: SourcePin, cancel: OwnedFd, attempted: bool, fenced: bool,
    instance: Option<uuid::Uuid>, slot: Option<uuid::Uuid>, ready: Option<(PathBuf, PathBuf)>,
    helpers: BTreeMap<uuid::Uuid, Process>, pending_helper: Option<Process>,
}
/// One enrolled original helper in a shared immutable seed session. IDs are
/// DATA; this non-Serde token must stay with that helper's actual task carrier.
pub struct EnrolledSeedHelper { instance: uuid::Uuid, slot: uuid::Uuid, reader: uuid::Uuid, pid: u32 }
impl ClientSeedOwner {
    /// OFF App capture, before queueing. The store stays UID1500-owned; this
    /// producer never opens it as root and never changes its ownership/modes.
    pub fn capture(store: LocalStore, already_retained_seed: ObjectRef) -> Result<Self> {
        service::floor(std::process::id(), 1500)?;
        Ok(Self { source: SourcePin::capture(store, already_retained_seed)?, cancel: service::eventfd()?,
            attempted: false, fenced: false, instance: None, slot: None, ready: None,
            helpers: BTreeMap::new(), pending_helper: None })
    }
    pub fn seed(&self) -> &ObjectRef { &self.source.seed }
    /// Returned paths are DATA only. An original live client and retained root
    /// reader still gate checked use; serialized paths cannot adopt this owner.
    pub fn paths(&self) -> Option<(&Path, &Path)> {
        if self.fenced { return None; }
        self.ready.as_ref().map(|(root, objects)| (root.as_path(), objects.as_path()))
    }
    pub fn cancel(&mut self) { self.fenced = true; service::cancel_event(&self.cancel); }
    /// Test-only duplicate of the original cancellation event, so the paired
    /// root fixture can cancel after actual Begin ACK while retaining this owner.
    #[cfg(test)]
    pub(crate) fn qualification_cancel_fd(&self) -> Result<OwnedFd> { Ok(self.cancel.try_clone()?) }
}

/// One original image-bootstrap connection in the actual UID1500 host engine.
/// No Clone/Serde/raw socket accessor. All IO/verification is OFF App.
pub struct RootStorageClient {
    endpoint: Endpoint, server: Process, own: Process, image: Image, engine_image: Image,
    _inherited_image: OwnedFd, mnt: File, user: File, boot: Vec<u8>, serial: u64,
    instance: Option<uuid::Uuid>, fenced: bool,
}
impl RootStorageClient {
    /// Bootstrap must supply the actual connected pair, original root-service
    /// pidfd and a held /proc/<service>/exe witness verified by root BEFORE drop.
    /// The current Docker/bootstrap does not issue these descriptors yet.
    pub fn from_bootstrap(endpoint: OwnedFd, original_service_pidfd: OwnedFd, original_service_image: OwnedFd) -> Result<Self> {
        ensure!(unsafe { libc::getuid() } == 1500 && unsafe { libc::geteuid() } == 1500
            && unsafe { libc::getgid() } == 1500 && unsafe { libc::getegid() } == 1500, "storage client requires actual UID/GID1500");
        service::floor(std::process::id(), 1500)?;
        // Same-UID host/legacy processes must not duplicate this privileged IPC
        // endpoint from /proc or ptrace the engine into proxying it for guests.
        ensure!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } == 0
            && unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } == 0, "storage host FD protection unavailable");
        service::fd_cloexec(&original_service_image)?;
        let flags = unsafe { libc::fcntl(original_service_image.as_raw_fd(), libc::F_GETFL) };
        ensure!(flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY, "storage bootstrap image witness must be readonly");
        let image = Image::capture(Path::new(SERVICE_IMAGE))?; image.matches_fd(&original_service_image)?;
        let own = Process::current()?; let engine_image = Image::capture(Path::new(ENGINE_IMAGE))?;
        engine_image.matches_process(&own)?;
        let value = Self { endpoint: Endpoint::capture(endpoint)?, server: Process::capture(original_service_pidfd)?, own,
            image, engine_image, _inherited_image: original_service_image,
            mnt: service::namespace(None, "mnt", libc::CLONE_NEWNS)?, user: service::namespace(None, "user", libc::CLONE_NEWUSER)?,
            boot: service::proc_bytes("/proc/sys/kernel/random/boot_id", 64)?, serial: 0, instance: None, fenced: false };
        ensure!(value.server.pid != value.own.pid, "storage client cannot authenticate itself as service"); value.current()?; Ok(value)
    }
    /// Fixed inherited FD adapter for the image-owned engine bootstrap only.
    /// No env-selected descriptor numbers or public native CLI route.
    pub fn from_inherited_bootstrap() -> Result<Self> {
        ensure!(!INHERITED_BOOTSTRAP_TAKEN.swap(true, Ordering::AcqRel), "storage inherited bootstrap already consumed");
        for fd in [3, 4, 5] { ensure!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0, "storage original bootstrap descriptor absent"); }
        Self::from_bootstrap(unsafe { OwnedFd::from_raw_fd(3) }, unsafe { OwnedFd::from_raw_fd(4) }, unsafe { OwnedFd::from_raw_fd(5) })
    }
    fn current(&self) -> Result<()> {
        ensure!(!self.fenced && self.own.pid == std::process::id(), "storage original client fenced/process changed");
        ensure!(unsafe { libc::getuid() } == 1500 && unsafe { libc::geteuid() } == 1500
            && unsafe { libc::getgid() } == 1500 && unsafe { libc::getegid() } == 1500, "storage client current thread identity changed");
        service::floor(self.own.pid, 1500)?;
        ensure!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } == 0, "storage engine descriptor protection changed");
        self.own.live()?; self.server.live()?; self.image.check()?; self.engine_image.matches_process(&self.own)?;
        ensure!(service::id(&self.mnt)? == service::id(&service::namespace(None, "mnt", libc::CLONE_NEWNS)?)?
            && service::id(&self.user)? == service::id(&service::namespace(None, "user", libc::CLONE_NEWUSER)?)?
            && self.boot == service::proc_bytes("/proc/sys/kernel/random/boot_id", 64)?, "storage client entered another host namespace/boot"); Ok(())
    }
    fn exchange(&mut self, request: Request, payload: &[u8], fd: Option<&OwnedFd>, budget: &FinalizationBudget) -> Result<Reply> {
        let result = (|| {
            self.current()?; budget.remaining()?;
            self.serial = self.serial.checked_add(1).context("storage client serial exhausted")?;
            let bytes = service::encode(&Envelope { version: 1, serial: self.serial, constraint: budget.constraint(), request }, payload)?;
            loop {
                self.current()?; budget.remaining()?;
                if self.endpoint.send(&bytes, fd)? { break; }
                std::thread::sleep(Duration::from_millis(2));
            }
            loop {
                self.current()?; budget.remaining()?;
                if let Some(packet) = self.endpoint.receive()? {
                    ensure!(packet.fds.is_empty() && packet.credentials.pid == self.server.pid as i32
                        && packet.credentials.uid == 0 && packet.credentials.gid == 0, "storage response is not original root service");
                    let (answer, payload): (Answer, _) = service::decode(&packet.bytes)?;
                    ensure!(answer.version == 1 && answer.serial == self.serial && payload.is_empty(), "storage response binding changed");
                    ensure!(!matches!(answer.reply, Reply::Rejected), "storage service rejected original operation");
                    self.current()?; budget.remaining()?; return Ok(answer.reply);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })();
        if result.is_err() { self.fenced = true; }
        result.map_err(|_| anyhow::anyhow!("root storage exchange failed; original owners retained"))
    }
    pub fn authenticate_original_service(&mut self, original: &FinalizationBudget) -> Result<()> {
        ensure!(self.instance.is_none(), "storage original service already authenticated");
        match self.exchange(Request::Hello { constraint: original.constraint() }, &[], None, original)? {
            Reply::Hello { instance, image_sha256 } => {
                if image_sha256 != self.image.sha256 { self.fenced = true; anyhow::bail!("storage original service image differs"); }
                self.instance = Some(instance); Ok(())
            }
            _ => { self.fenced = true; anyhow::bail!("storage original service handshake invalid") }
        }
    }
    /// Exactly one preparation attempt. Borrowed owners remain available on all
    /// failures, including source pin/remote import/mount/ACK uncertainty.
    pub fn prepare_seed_retaining(&mut self, owner: &mut ClientSeedOwner, original: &FinalizationBudget) -> Result<()> {
        ensure!(!owner.attempted && !owner.fenced, "storage original seed already attempted/fenced");
        owner.attempted = true;
        let result = (|| {
            self.current()?; original.remaining()?;
            let instance = self.instance.context("storage original service authentication required")?;
            owner.instance = Some(instance); owner.source.acquire()?; original.remaining()?;
            let refs = bounded_inventory(&owner.source.seed, original, |reference| {
                owner.source.check()?; owner.source.store.references(reference)
            })?;
            let mut total = 0u64;
            ensure!(!refs.is_empty() && refs.len() <= MAX_REFS, "storage source closure exceeds ref bound");
            for reference in &refs {
                service::object_bound(reference, MAX_OBJECT_BYTES)?;
                total = total.checked_add(reference.size_bytes).context("storage source byte overflow")?;
            }
            ensure!(total > 0 && total <= MAX_TRANSFER_BYTES, "storage source transfer exceeds byte bound");
            match self.exchange(Request::Begin { constraint: original.constraint(), seed: owner.source.seed.clone(), refs: refs.len(), bytes: total },
                &[], Some(&owner.cancel), original)? {
                Reply::Begun { slot } => owner.slot = Some(slot), _ => anyhow::bail!("storage original begin acknowledgement invalid"),
            }
            let slot = owner.slot.unwrap();
            for reference in refs {
                original.remaining()?; owner.source.check()?;
                let bytes = owner.source.store.read_object(&reference)?;
                original.remaining()?;
                ensure!(matches!(self.exchange(Request::Object { slot, reference }, &[], None, original)?, Reply::Accepted), "storage object acknowledgement invalid");
                if bytes.is_empty() {
                    ensure!(matches!(self.exchange(Request::Chunk { slot, offset: 0 }, &[], None, original)?, Reply::Accepted), "storage empty object acknowledgement invalid");
                } else {
                    for (i, chunk) in bytes.chunks(CHUNK).enumerate() {
                        ensure!(matches!(self.exchange(Request::Chunk { slot, offset: i * CHUNK }, chunk, None, original)?, Reply::Accepted), "storage chunk acknowledgement invalid");
                    }
                }
            }
            match self.exchange(Request::Commit { slot }, &[], None, original)? {
                Reply::Ready { slot: actual, seed, root, objects } => {
                    ensure!(actual == slot && seed == owner.source.seed && root.is_absolute()
                        && root.starts_with("/run/rudder-managed-seeds") && root.file_name().is_some_and(|name| name == "seed")
                        && objects == root.join("objects"), "storage original sealed paths differ");
                    owner.ready = Some((root, objects));
                }
                _ => anyhow::bail!("storage root producer acknowledgement invalid"),
            }
            owner.source.check()?; self.current()?; original.remaining()?; Ok(())
        })();
        if result.is_err() { owner.cancel(); self.fenced = true; }
        result.map_err(|_| anyhow::anyhow!("root seed preparation failed; original transfer/import/mount owners retained"))
    }
    fn original_slot(&self, owner: &ClientSeedOwner) -> Result<uuid::Uuid> {
        ensure!(!owner.fenced && owner.instance == self.instance && self.instance.is_some() && owner.ready.is_some(), "storage original read owner unavailable");
        owner.source.check()?; owner.slot.context("storage original slot absent")
    }
    /// Checked reuse of production verifier with the user's own retained CAS.
    /// No root retention writes can change that CAS's UID1500 ownership.
    pub fn open_retained_template(&mut self, owner: &ClientSeedOwner, operation: &FinalizationBudget) -> Result<VerifiedMetadataTemplate> {
        let slot = self.original_slot(owner)?;
        ensure!(matches!(self.exchange(Request::Check { slot }, &[], None, operation)?, Reply::Accepted), "storage original reader check failed");
        let (root, objects) = owner.ready.as_ref().unwrap();
        let template = VerifiedMetadataTemplate::open_retained(&owner.source.store, &owner.source.seed, root, objects)?;
        self.current()?; operation.remaining()?; Ok(template)
    }
    /// OFF App: caller passes a DUPLICATE of its actual retained original helper
    /// witness pidfd, never opens one from a saved PID. The current helper witness
    /// lacks that export API; a separate mechanical host adapter is still needed.
    pub fn enroll_original_helper_retaining(&mut self, owner: &mut ClientSeedOwner, original_helper_pidfd: OwnedFd, operation: &FinalizationBudget) -> Result<EnrolledSeedHelper> {
        let slot = self.original_slot(owner)?;
        ensure!(owner.pending_helper.is_none() && owner.helpers.len() < MAX_HELPERS_PER_SEED, "storage original helper reader admission full/uncertain");
        owner.pending_helper = Some(Process::capture(original_helper_pidfd)?);
        let result = (|| {
            let actual = owner.pending_helper.as_ref().unwrap();
            match self.exchange(Request::EnrollHelper { slot }, &[], Some(actual.fd()), operation)? {
                Reply::HelperEnrolled { reader, pid } => {
                    ensure!(pid == actual.pid && !owner.helpers.contains_key(&reader), "storage original helper reader acknowledgement differs");
                    owner.helpers.insert(reader, owner.pending_helper.take().unwrap());
                    Ok(EnrolledSeedHelper { instance: self.instance.unwrap(), slot, reader, pid })
                }
                _ => anyhow::bail!("storage original helper acknowledgement invalid"),
            }
        })();
        if result.is_err() { owner.cancel(); self.fenced = true; }
        result
    }
    /// Physical helper exit is a separate fact. No lease/ACK/message can replace it.
    pub fn enrolled_helper_exited(&self, owner: &ClientSeedOwner, original: &EnrolledSeedHelper) -> Result<bool> {
        self.current()?; self.original_slot(owner)?;
        ensure!(Some(original.instance) == owner.instance && Some(original.slot) == owner.slot,
            "storage helper token belongs to another original seed");
        let helper = owner.helpers.get(&original.reader).context("storage original helper reader absent")?;
        ensure!(helper.pid == original.pid, "storage original helper token changed"); helper.exited()
    }
    pub fn all_enrolled_helpers_exited(&self, owner: &ClientSeedOwner) -> Result<bool> {
        self.current()?; self.original_slot(owner)?;
        ensure!(owner.pending_helper.is_none() && !owner.helpers.is_empty(), "storage original helper enrollment/drain unknown");
        for helper in owner.helpers.values() { if !helper.exited()? { return Ok(false); } }
        Ok(true)
    }
    /// Explicit gate: no drained:boolean/TTL retirement opcode exists. Root must
    /// wire actual App/Broker/Runtime join custody to the private service issuer.
    pub fn request_retirement_retaining(&mut self, _owner: &mut ClientSeedOwner, _operation: &FinalizationBudget) -> Result<()> {
        anyhow::bail!("root storage retirement requires the missing typed native actor-drain bridge; original owners retained")
    }
}
