//! Trusted host-engine client only. No public CLI or path/JSON mount-adoption
//! API. Keep actual transfer/client owners outside worker/result callbacks.
#![cfg(target_os = "linux")]
use anyhow::{ensure, Context, Result};
use sha2::Digest;
use crate::root_storage_service::{self as service, Answer, Endpoint, Envelope, Image, Process, Reply, Request,
    CHUNK, ENGINE_IMAGE, MAX_HELPERS_PER_SEED, MAX_OBJECT_BYTES, MAX_REFS, MAX_TRANSFER_BYTES, SERVICE_IMAGE};
use rudder_engine::{execution::finalization_budget::FinalizationBudget,
    protocol::{ObjectKind, ObjectRef}, store::LocalStore};
use rudder_vcs_bridge::seed::{VerifiedMetadataTemplate,LocalizedMetadataFork,NeverLinkedMetadataFork};
use crate::root_storage_uses::{SeedUseLedger,SeedUseGuard,SeedUseKind,SeedActorsJoined,RetainedLocalizedFork,SeedTaskUseGate,SealedSeedTaskUse,RetainedUnusedFork};
use std::sync::{Arc,Weak};
use std::{collections::{BTreeMap, BTreeSet}, fs::{self, File, OpenOptions}, os::{fd::{AsRawFd, FromRawFd, OwnedFd}, unix::fs::{MetadataExt, OpenOptionsExt}},
    path::{Path, PathBuf}, sync::atomic::{AtomicBool, Ordering}, time::Duration};
static INHERITED_BOOTSTRAP_TAKEN: AtomicBool = AtomicBool::new(false);

/// The source is already pinned by the caller. Check every newly discovered
/// typed reference BEFORE reading it or growing the retained transfer queue.
pub(crate) fn bounded_inventory(root: &ObjectRef, original: &FinalizationBudget,
    mut references: impl FnMut(&ObjectRef) -> Result<Vec<ObjectRef>>) -> Result<Vec<ObjectRef>> {
    bounded_inventory_profile(rudder_vcs_bridge::seed::SeedProfile::SmallV1,root,original,references)
}
fn bounded_inventory_profile(profile:rudder_vcs_bridge::seed::SeedProfile,root:&ObjectRef,original:&FinalizationBudget,
    mut references:impl FnMut(&ObjectRef)->Result<Vec<ObjectRef>>)->Result<Vec<ObjectRef>>{
    let paged=profile==rudder_vcs_bridge::seed::SeedProfile::PagedCowV2;
    let max_refs=if paged{rudder_vcs_bridge::seed::paged::V2_MAX_REFS}else{MAX_REFS};
    let max_bytes=if paged{rudder_vcs_bridge::seed::paged::V2_MAX_CAS_BYTES}else{MAX_TRANSFER_BYTES};
    if paged{service::paged::bound(root,max_bytes)?;}else{service::object_bound(root,max_bytes)?;}

    let mut known = BTreeSet::from([root.clone()]); let mut pending = vec![root.clone()]; let mut total = root.size_bytes;
    while let Some(reference) = pending.pop() {
        original.remaining()?;
        let children = references(&reference)?;
        original.remaining()?;
        for child in children {
            if known.contains(&child) { continue; }
            ensure!(known.len() < max_refs, "storage source closure exceeds ref bound");
            if paged{service::paged::bound(&child,max_bytes-total)?;}else{service::object_bound(&child,max_bytes-total)?;}
            total += child.size_bytes; known.insert(child.clone()); pending.push(child);
        }
    }
    ensure!(total > 0, "storage source closure is empty"); original.remaining()?;
    Ok(known.into_iter().collect())
}

struct SourcePin {
    store: LocalStore, directory: File, identity: (u64, u64), seed: ObjectRef, pin: String,
    attempted: bool, acquired: bool, unpin_attempted:bool, removed:bool,
}
impl SourcePin {
    fn capture(store: LocalStore, seed: ObjectRef) -> Result<Self> {
        seed.validate()?; ensure!(seed.kind == ObjectKind::Directory && seed.size_bytes <= 256 * 1024, "storage client seed exceeds small profile");
        let directory = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(store.root())?;
        let pin = Self { identity: service::id(&directory)?, directory, store, seed,
            pin: format!("root-storage-client:{}", uuid::Uuid::new_v4()), attempted: false, acquired: false,unpin_attempted:false,removed:false };
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
    fn release_retaining(&mut self,original:&FinalizationBudget)->Result<()> {
        if self.removed {return Ok(());}original.remaining()?;self.current_directory()?;
        match self.store.retention()?.pins.get(&self.pin) {
            Some(root)=>{ensure!(root==&self.seed,"original client pin changed");self.unpin_attempted=true;self.store.unpin(&self.pin)?;}
            None=>ensure!(self.unpin_attempted,"client pin disappeared before original unpin intent"),
        }
        ensure!(!self.store.retention()?.pins.contains_key(&self.pin),"original client pin removal uncertain");
        self.current_directory()?;original.remaining()?;self.removed=true;Ok(())
    }

}

/// Original client-side transfer/read owner. Not Clone/Serde, and Drop never
/// unpins. Errors retain the actual source pin, cancel event and helper pidfd.
#[must_use = "retain through original helper and actor drain plus root retirement acknowledgement"]
pub struct ClientSeedOwner {
    source: SourcePin, cancel: OwnedFd, attempted: bool, fenced: bool,
    instance: Option<uuid::Uuid>, slot: Option<uuid::Uuid>, ready: Option<(PathBuf, PathBuf)>,cow_floor:Option<ObjectRef>,
    helpers: BTreeMap<uuid::Uuid, Process>, pending_helper: Option<Process>,
    uses:SeedUseLedger,templates:Vec<Weak<VerifiedMetadataTemplate>>,untracked_templates:AtomicBool,
    retirement:Option<ClientRetirement>,paged_transfer:Option<seed_window::SeedPagedTransfer>,original_cancel:Option<Arc<AtomicBool>>,enrollment_cow:Option<String>,profile:rudder_vcs_bridge::seed::SeedProfile,
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
            attempted: false, fenced: false, instance: None, slot: None, ready: None,cow_floor:None,
            helpers: BTreeMap::new(), pending_helper: None,uses:SeedUseLedger::new(),templates:Vec::with_capacity(2),
            untracked_templates:AtomicBool::new(false),retirement:None,paged_transfer:None,original_cancel:None,enrollment_cow:None,profile:rudder_vcs_bridge::seed::SeedProfile::SmallV1 })
    }
    /// Explicit V2 from the actual already acknowledged source capsule. No
    /// profile auto-detection/readback promotion and no V1 fallback.
    pub fn capture_paged(store:LocalStore,already_retained_seed:ObjectRef,original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<Self>{
        let mut owner=Self::capture(store,already_retained_seed)?;
        rudder_vcs_bridge::seed::read_paged_seed(&owner.source.store,&owner.source.seed,||{if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("paged original capture cancelled");}original.remaining()?;Ok(())})?;
        owner.original_cancel=Some(cancelled);owner.profile=rudder_vcs_bridge::seed::SeedProfile::PagedCowV2;Ok(owner)
    }
    pub fn profile(&self)->rudder_vcs_bridge::seed::SeedProfile {self.profile}
    pub fn seed(&self) -> &ObjectRef { &self.source.seed }
    /// Returned paths are DATA only. An original live client and retained root
    /// reader still gate checked use; serialized paths cannot adopt this owner.
    pub fn paths(&self) -> Option<(&Path, &Path)> {
        if self.fenced||self.retirement.is_some() { return None; }
        self.ready.as_ref().map(|(root, objects)| (root.as_path(), objects.as_path()))
    }
    /// OFF App. Stage guard BEFORE use; late failure leaves the actual guard in
    /// caller storage. Dropping it keeps its unresolved ledger entry charged.
    pub fn acquire_use_retaining(&self,slot:&mut Option<SeedUseGuard>,kind:SeedUseKind,original:&FinalizationBudget)->Result<()> {
        original.remaining()?;ensure!(slot.is_none()&&!self.fenced&&self.ready.is_some()&&self.retirement.is_none(),"original seed use unavailable");
        self.source.check()?;self.uses.admit()?;*slot=Some(self.uses.acquire(kind)?);original.remaining()?;Ok(())
    }
    pub fn register_task_start_retaining(&self,out:&mut Option<SeedTaskUseGate>,task:&SeedUseGuard,
        fork:&SeedUseGuard,original:&FinalizationBudget)->Result<()> {
        original.remaining()?;ensure!(!self.fenced&&self.ready.is_some()&&self.retirement.is_none(),"original seed unavailable");
        self.source.check()?;self.uses.register_task_start_retaining(out,task,fork,self.seed(),original)?;original.remaining()?;Ok(())
    }
    pub fn close_native_admission(&self)->Result<()> {self.uses.close()}
    /// Compatibility refusal only; this cannot grant or clear any authority.
    pub fn has_untracked_native_use(&self)->bool {self.untracked_templates.load(Ordering::Acquire)}
    pub fn latch_untracked_native_use(&self) {self.untracked_templates.store(true,Ordering::Release);}
    /// OFF App. Joins original handles even if a typed result was dropped;
    /// failed/unknown callbacks remain charged and their actual T is retained.
    pub fn poll_native_joins_off_app(&self)->Result<()> {self.uses.drive_joins_off_app()}
    pub fn retirement_ack(&self)->Option<&RootSeedRetired> {self.retirement.as_ref()?.ack.as_ref()}
    pub fn is_retired(&self)->bool {self.retirement.as_ref().is_some_and(|intent|intent.released&&self.source.removed)}
    pub fn cancel(&mut self) { self.fenced = true; service::cancel_event(&self.cancel); }
    /// Test-only duplicate of the original cancellation event, so the paired
    /// root fixture can cancel after actual Begin ACK while retaining this owner.
    #[cfg(test)]
    pub(crate) fn qualification_cancel_fd(&self) -> Result<OwnedFd> { Ok(self.cancel.try_clone()?) }
}

/// Past original Root cleanup ACK, not a serialized reconstruction or read grant.
#[must_use="retain with original actor/client/seed owners"]
pub struct RootSeedRetired {_instance:uuid::Uuid,_slot:uuid::Uuid,_operation:uuid::Uuid}
struct RetirementPacket {serial:u64,bytes:Vec<u8>,release:bool,attempted:bool,replayed:bool}
enum RetirementReply {None,Duplicate,Accepted,Partial}
struct ClientRetirement {paged:bool,id:uuid::Uuid,slot:uuid::Uuid,original:FinalizationBudget,_actors:SeedActorsJoined,
    readers:Vec<String>,packet:Option<RetirementPacket>,last_reply:Option<(u64,Vec<u8>)>,ack:Option<RootSeedRetired>,released:bool}

/// One original image-bootstrap connection in the actual UID1500 host engine.
/// No Clone/Serde/raw socket accessor. All IO/verification is OFF App.
pub struct RootStorageClient {
    endpoint: Endpoint, server: Process, own: Process, image: Image, engine_image: Image,
    _inherited_image: OwnedFd, mnt: File, user: File, boot: Vec<u8>, serial: u64,
    instance: Option<uuid::Uuid>, fenced: bool,retirement_inflight:Option<uuid::Uuid>,cow_inflight:Option<String>,seed_window_inflight:Option<uuid::Uuid>,private_v2_recovery:Option<RootPrivateEffect>,generic_inflight:Option<GenericOriginal>,last_generic:Option<GenericOriginal>,
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
            boot: service::proc_bytes("/proc/sys/kernel/random/boot_id", 64)?, serial: 0, instance: None, fenced: false,retirement_inflight:None,cow_inflight:None,seed_window_inflight:None,private_v2_recovery:None,generic_inflight:None,last_generic:None };
        ensure!(value.server.pid != value.own.pid, "storage client cannot authenticate itself as service"); value.current()?; Ok(value)
    }
    /// Fixed inherited FD adapter for the image-owned engine bootstrap only.
    /// No env-selected descriptor numbers or public native CLI route.
    pub fn from_inherited_bootstrap() -> Result<Self> {
        ensure!(!INHERITED_BOOTSTRAP_TAKEN.swap(true, Ordering::AcqRel), "storage inherited bootstrap already consumed");
        for fd in [3, 4, 5] { ensure!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0, "storage original bootstrap descriptor absent"); }
        Self::from_bootstrap(unsafe { OwnedFd::from_raw_fd(3) }, unsafe { OwnedFd::from_raw_fd(4) }, unsafe { OwnedFd::from_raw_fd(5) })
    }
    fn current(&self)->Result<()>{ensure!(!self.fenced,"storage original client fenced");self.current_physical()}
    fn current_physical(&self) -> Result<()> {
        ensure!(self.own.pid == std::process::id(), "storage original client fenced/process changed");
        ensure!(unsafe { libc::getuid() } == 1500 && unsafe { libc::geteuid() } == 1500
            && unsafe { libc::getgid() } == 1500 && unsafe { libc::getegid() } == 1500, "storage client current thread identity changed");
        service::floor(self.own.pid, 1500)?;
        ensure!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } == 0, "storage engine descriptor protection changed");
        self.own.live()?; self.server.live()?; self.image.check()?; self.engine_image.matches_process(&self.own)?;
        ensure!(service::id(&self.mnt)? == service::id(&service::namespace(None, "mnt", libc::CLONE_NEWNS)?)?
            && service::id(&self.user)? == service::id(&service::namespace(None, "user", libc::CLONE_NEWUSER)?)?
            && self.boot == service::proc_bytes("/proc/sys/kernel/random/boot_id", 64)?, "storage client entered another host namespace/boot"); Ok(())
    }
    pub(crate) fn assert_authenticated_bootstrap_current(&self)->Result<()> {
        self.current()?;ensure!(self.instance.is_some(),"storage original service authentication absent");Ok(())
    }
    /// OFF App physical/client continuity only. No seed/backend readiness or
    /// new read grant is created; caller must still hold its actual seed scope.
    pub fn assert_authenticated_current(&self)->Result<()> {self.assert_authenticated_bootstrap_current()}
    /// Refusal-state observation only, never a retirement/settlement ACK.
    pub fn retirement_pending(&self)->bool {self.retirement_inflight.is_some()}
    fn exchange(&mut self,request:Request,payload:&[u8],fd:Option<&OwnedFd>,budget:&FinalizationBudget)->Result<Reply>{
        ensure!(self.retirement_inflight.is_none()&&self.cow_inflight.is_none()&&self.seed_window_inflight.is_none()&&self.generic_inflight.is_none(),"original Root ACK reconciliation pending; no interleaved RPC");
        let result=(||{
            self.current()?;budget.remaining()?;let serial=self.serial.checked_add(1).context("storage serial exhausted")?;
            let bytes=service::encode(&Envelope{version:1,serial,constraint:budget.constraint(),request},payload)?;
            let fd=fd.map(OwnedFd::try_clone).transpose()?;
            self.generic_inflight=Some(GenericOriginal{serial,bytes,fd,packet:None});self.serial=serial;
            loop{self.current()?;budget.remaining()?;let rpc=self.generic_inflight.as_ref().unwrap();
                if self.endpoint.send(&rpc.bytes,rpc.fd.as_ref())?{break}std::thread::sleep(budget.phase_limit(Duration::from_millis(2))?);}
            loop{self.current()?;budget.remaining()?;
                let rpc=self.generic_inflight.as_mut().unwrap();if rpc.packet.is_none(){rpc.packet=self.endpoint.receive()?;}
                if let Some(packet)=&rpc.packet{
                    let retained=self.private_v2_recovery.take();
                    ensure!(packet.fds.is_empty()&&packet.credentials.pid==self.server.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0,"storage original reply credentials differ");
                    let(answer,payload):(Answer,_)=service::decode(&packet.bytes)?;
                    ensure!(answer.version==1&&answer.serial==rpc.serial&&payload.is_empty(),"storage original reply order differs");
                    self.private_v2_recovery=retained;self.last_generic=self.generic_inflight.take();
                    ensure!(!matches!(answer.reply,Reply::Rejected),"storage service rejected original operation");
                    self.current()?;budget.remaining()?;return Ok(answer.reply)
                }std::thread::sleep(budget.phase_limit(Duration::from_millis(2))?);
            }
        })();if result.is_err(){self.fenced=true;}result.map_err(|_|anyhow::anyhow!("root storage exchange failed; original owners retained"))
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
        ensure!(self.retirement_inflight.is_none(),"original retirement reconciliation pending");
        ensure!(!owner.attempted && !owner.fenced, "storage original seed already attempted/fenced");
        owner.attempted = true;
        if owner.profile==rudder_vcs_bridge::seed::SeedProfile::PagedCowV2{self.private_v2_recovery=Some(RootPrivateEffect::SeedPreparation);}
        let result = (|| {
            self.current()?; original.remaining()?;
            let instance = self.instance.context("storage original service authentication required")?;
            owner.instance = Some(instance); owner.source.acquire()?; original.remaining()?;
            let refs = bounded_inventory_profile(owner.profile,&owner.source.seed, original, |reference| {
                owner.source.check()?; owner.source.store.references(reference)
            })?;
            let mut total = 0u64;
            let paged=owner.profile==rudder_vcs_bridge::seed::SeedProfile::PagedCowV2;
            ensure!(!refs.is_empty() && refs.len() <= if paged {rudder_vcs_bridge::seed::paged::V2_MAX_REFS}else{MAX_REFS}, "storage source closure exceeds ref bound");
            for reference in &refs {
                if paged {service::paged::bound(reference,4<<20)?;}else{service::object_bound(reference,MAX_OBJECT_BYTES)?;}
                total = total.checked_add(reference.size_bytes).context("storage source byte overflow")?;
            }
            ensure!(total > 0 && total <= if paged {rudder_vcs_bridge::seed::paged::V2_MAX_CAS_BYTES}else{MAX_TRANSFER_BYTES}, "storage source transfer exceeds byte bound");
            let begin=if paged {Request::BeginPaged{constraint:original.constraint(),seed:owner.source.seed.clone(),refs:refs.len(),bytes:total}}
                else{Request::Begin{constraint:original.constraint(),seed:owner.source.seed.clone(),refs:refs.len(),bytes:total}};
            match self.exchange(begin,
                &[], Some(&owner.cancel), original)? {
                Reply::Begun { slot } => owner.slot = Some(slot), _ => anyhow::bail!("storage original begin acknowledgement invalid"),
            }
            let slot = owner.slot.unwrap();
            if paged {self.transfer_paged_seed_retaining(owner,refs,original)?;}
            else {for reference in refs {
                original.remaining()?; owner.source.check()?;
                let reply=self.exchange(Request::Object{slot,reference:reference.clone()},&[],None,original)?;
                if paged&&matches!(reply,Reply::ObjectPresent){continue;}
                ensure!(matches!(reply,Reply::Accepted),"storage original object ACK invalid");
                let bytes=owner.source.store.read_object(&reference)?;original.remaining()?;
                if bytes.is_empty() {
                    ensure!(matches!(self.exchange(Request::Chunk { slot, offset: 0 }, &[], None, original)?, Reply::Accepted), "storage empty object acknowledgement invalid");
                } else {
                    for (i, chunk) in bytes.chunks(CHUNK).enumerate() {
                        ensure!(matches!(self.exchange(Request::Chunk { slot, offset: i * CHUNK }, chunk, None, original)?, Reply::Accepted), "storage chunk acknowledgement invalid");
                    }
                }
            }
            }
            match self.exchange(Request::Commit { slot }, &[], None, original)? {
                Reply::PagedReady{slot:actual,seed,root,objects,cow_floor}=>{
                    owner.ready=Some((root.clone(),objects.clone()));owner.cow_floor=Some(cow_floor.clone());
                    ensure!(paged&&actual==slot&&seed==owner.source.seed&&root.is_absolute()&&root.starts_with("/run/rudder-managed-seeds")
                        &&root.file_name().is_some_and(|n|n=="seed")&&objects==root.join("objects")&&cow_floor.kind==ObjectKind::Directory&&cow_floor.size_bytes<=4096,"original paged Root ready ACK differs");
                }
                Reply::Ready { slot: actual, seed, root, objects } => {
                    ensure!(!paged,"V2 cannot adopt a V1 Root reply");
                    owner.ready=Some((root.clone(),objects.clone()));
                    ensure!(actual == slot && seed == owner.source.seed && root.is_absolute()
                        && root.starts_with("/run/rudder-managed-seeds") && root.file_name().is_some_and(|name| name == "seed")
                        && objects == root.join("objects"), "storage original sealed paths differ");
                    owner.ready = Some((root, objects));
                }
                _ => anyhow::bail!("storage root producer acknowledgement invalid"),
            }
            owner.source.check()?; self.current()?; original.remaining()?; Ok(())
        })();
        if result.is_err() { owner.cancel(); self.fenced = true; }else{self.private_v2_recovery=None;}
        result.map_err(|_| anyhow::anyhow!("root seed preparation failed; original transfer/import/mount owners retained"))
    }
    fn original_slot(&self, owner: &ClientSeedOwner) -> Result<uuid::Uuid> {
        ensure!(!owner.fenced && owner.instance == self.instance && self.instance.is_some() && owner.ready.is_some(), "storage original read owner unavailable");
        owner.source.check()?; owner.slot.context("storage original slot absent")
    }
    /// Checked reuse of production verifier with the user's own retained CAS.
    /// No root retention writes can change that CAS's UID1500 ownership.
    pub fn open_retained_template(&mut self, owner: &ClientSeedOwner, operation: &FinalizationBudget) -> Result<VerifiedMetadataTemplate> {
        // Compatibility only: callers using an untracked template may still
        // run, but this owner can never later claim complete native drain.
        owner.uses.admit()?;owner.untracked_templates.store(true,Ordering::Release);
        let slot = self.original_slot(owner)?;
        ensure!(matches!(self.exchange(Request::Check { slot }, &[], None, operation)?, Reply::Accepted), "storage original reader check failed");
        let (root, objects) = owner.ready.as_ref().unwrap();
        let template = if owner.profile==rudder_vcs_bridge::seed::SeedProfile::PagedCowV2 {
            VerifiedMetadataTemplate::open_paged_retained_bounded(&owner.source.store,&owner.source.seed,root,objects,operation,owner.original_cancel.as_ref().context("original V2 cancel carrier absent")?)?
        }else{VerifiedMetadataTemplate::open_retained(&owner.source.store,&owner.source.seed,root,objects)?};
        self.current()?; operation.remaining()?; Ok(template)
    }
    /// Actual shared Arc aliases are tracked. Native fork/cache scopes must
    /// also retain registered guards through their actual actor/TLS joins.
    pub fn open_tracked_template(&mut self,owner:&mut ClientSeedOwner,guard:&SeedUseGuard,original:&FinalizationBudget)->Result<Arc<VerifiedMetadataTemplate>> {
        original.remaining()?;owner.uses.admit()?;owner.uses.require_kind(guard,SeedUseKind::Template)?;
        let slot=self.original_slot(owner)?;
        ensure!(matches!(self.exchange(Request::Check {slot},&[],None,original)?,Reply::Accepted),"original root reader check failed");
        owner.templates.retain(|weak|weak.strong_count()>0);
        ensure!(owner.templates.len()<2,"tracked original template admission full");
        let (root,objects)=owner.ready.as_ref().context("original paths unavailable")?;
        let template=Arc::new(if owner.profile==rudder_vcs_bridge::seed::SeedProfile::PagedCowV2 {
            VerifiedMetadataTemplate::open_paged_retained_bounded(&owner.source.store,&owner.source.seed,root,objects,original,owner.original_cancel.as_ref().context("original V2 cancel carrier absent")?)?
        }else{VerifiedMetadataTemplate::open_retained(&owner.source.store,&owner.source.seed,root,objects)?});
        owner.templates.push(Arc::downgrade(&template));self.current()?;original.remaining()?;Ok(template)
    }
    pub fn associate_original_helper_use(&self,owner:&ClientSeedOwner,guard:&SeedUseGuard,helper:&EnrolledSeedHelper)->Result<()> {
        self.original_slot(owner)?;
        ensure!(owner.instance==Some(helper.instance)&&owner.slot==Some(helper.slot),"helper ACK belongs to another original seed");
        ensure!(owner.helpers.get(&helper.reader).is_some_and(|actual|actual.pid==helper.pid),"original helper ACK/process absent");
        owner.uses.associate(guard,helper.reader)
    }
    /// Consume the actual non-Serde VCS localizer ACK ONCE into BOTH original
    /// task/fork uses. Binding checks against the genuine Joined task occur in
    /// the fixed native bridge before this call; Root still checks its pidfds.
    pub fn acknowledge_localized_fork_retaining(&mut self,owner:&ClientSeedOwner,out:&mut Option<RetainedLocalizedFork>,
        task:&SeedUseGuard,fork:&SeedUseGuard,proof:&mut Option<LocalizedMetadataFork>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.current()?;self.original_slot(owner)?;original.remaining()?;
        let actual=proof.as_ref().context("original opaque fork localization ACK absent")?;
        ensure!(actual.kernel_readonly_objects()&&actual.seed()==owner.seed()&&owner.ready.as_ref().is_some_and(|(_,objects)|objects==actual.original_shared_objects_root()),
            "localization ACK belongs to another original seed/mount");
        owner.uses.acknowledge_localized_retaining(out,task,fork,proof,original,cancelled)?;
        self.current()?;original.remaining()?;Ok(())
    }
    /// Genuine unused task admission plus original VCS dependency-release ACK.
    /// Per-task close allows siblings to continue; whole seed close is still
    /// mandatory before actual Root retirement.
    pub fn acknowledge_never_started_fork_retaining(&mut self,owner:&ClientSeedOwner,out:&mut Option<RetainedUnusedFork>,
        task:&SeedUseGuard,fork:&SeedUseGuard,seal:&SealedSeedTaskUse,localized:&mut Option<LocalizedMetadataFork>,
        never_linked:&mut Option<NeverLinkedMetadataFork>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.current()?;self.original_slot(owner)?;original.remaining()?;
        ensure!(seal.seed()==owner.seed(),"never-started seal belongs to another seed");
        if let Some(proof)=localized.as_ref() {ensure!(proof.kernel_readonly_objects()&&proof.seed()==owner.seed()
            &&owner.ready.as_ref().is_some_and(|(_,objects)|objects==proof.original_shared_objects_root()),"unused localization uses another original mount");}
        if let Some(proof)=never_linked.as_ref() {ensure!(proof.seed()==owner.seed()
            &&owner.ready.as_ref().is_some_and(|(_,objects)|objects==proof.original_shared_objects_root()),"never-linked receipt uses another original mount");}
        owner.uses.acknowledge_never_started_retaining(out,task,fork,seal,localized,never_linked,original,cancelled)?;
        self.current()?;original.remaining()?;Ok(())
    }
    /// OFF App: caller passes a DUPLICATE of its actual retained original helper
    /// witness pidfd, never opens one from a saved PID. The current helper witness
    /// lacks that export API; a separate mechanical host adapter is still needed.
    pub fn enroll_original_helper_retaining(&mut self, owner: &mut ClientSeedOwner, original_helper_pidfd: OwnedFd, operation: &FinalizationBudget) -> Result<EnrolledSeedHelper> {
        ensure!(self.retirement_inflight.is_none(),"original retirement reconciliation pending");owner.uses.admit()?;
        let slot = self.original_slot(owner)?;
        ensure!(owner.pending_helper.is_none() && owner.helpers.len() < MAX_HELPERS_PER_SEED, "storage original helper reader admission full/uncertain");
        owner.pending_helper = Some(Process::capture(original_helper_pidfd)?);
        if owner.profile==rudder_vcs_bridge::seed::SeedProfile::PagedCowV2{self.private_v2_recovery=Some(RootPrivateEffect::CowEnrollment);}
        let result = (|| {
            let actual = owner.pending_helper.as_ref().unwrap();
            let request=if owner.profile==rudder_vcs_bridge::seed::SeedProfile::PagedCowV2 {
                Request::EnrollHelperCow{slot,nonce:owner.enrollment_cow.clone().context("original V2 CoW enrollment context absent")?}
            }else{Request::EnrollHelper{slot}};
            match self.exchange(request, &[], Some(actual.fd()), operation)? {
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
    /// OFF App. Close admission before proving EVERY actual native use. No
    /// bool/JSON/TTL/path can issue this proof; dropped/unknown guards block it.
    /// Once admitted, every retry uses this SAME retained original budget.
    pub fn request_retirement_retaining(&mut self,owner:&mut ClientSeedOwner,operation:&FinalizationBudget)->Result<()> {
        self.current_for_retained_cleanup(owner)?;
        ensure!(self.retirement_inflight.is_none()||owner.retirement.as_ref().is_some_and(|intent|Some(intent.id)==self.retirement_inflight),"another original retirement requires reconciliation");
        if owner.retirement.is_none() {
            operation.remaining()?;owner.uses.close()?;
            ensure!(!owner.untracked_templates.load(Ordering::Acquire)&&owner.templates.iter().all(|weak|weak.upgrade().is_none()),"legacy/unreleased native template aliases block retirement");
            ensure!(owner.pending_helper.is_none(),"unknown enrollment ACK blocks retirement");
            let slot=self.original_slot(owner)?;
            let actors=owner.uses.prove(&owner.helpers.keys().copied().collect(),operation,&AtomicBool::new(false))?;
            for helper in owner.helpers.values(){ensure!(helper.exited()?,"original sibling helper still live");}
            operation.remaining()?;
            owner.retirement=Some(ClientRetirement {paged:owner.profile==rudder_vcs_bridge::seed::SeedProfile::PagedCowV2,id:uuid::Uuid::new_v4(),slot,original:operation.clone(),_actors:actors,
                readers:owner.helpers.keys().map(ToString::to_string).collect(),packet:None,last_reply:None,ack:None,released:false});
        }
        let intent=owner.retirement.as_mut().unwrap();
        ensure!(intent.original.constraint()==operation.constraint(),"retirement original budget changed");
        self.retirement_inflight=Some(intent.id);
        if intent.released {self.retirement_inflight=None;return Ok(());}
        intent.original.remaining()?;
        if intent.ack.is_none() {
            self.retirement_exchange(intent,false)?;intent.original.remaining()?;
        }
        owner.source.release_retaining(&intent.original)?;intent.original.remaining()?;
        self.retirement_exchange(intent,true)?;
        ensure!(intent.released,"original Root capacity-release ACK absent");
        self.retirement_inflight=None;Ok(())
    }
    /// Explicit original-packet reconciliation. An unknown packet is retried
    /// at most once, byte-for-byte/same serial/budget. Root replays only its held
    /// exact ACK; no status/path readback becomes authority. Errors keep owner.
    fn retirement_exchange(&mut self,intent:&mut ClientRetirement,release:bool)->Result<()> {
        self.current_for_retirement_intent(intent)?;intent.original.remaining()?;
        if intent.packet.is_none() {
            let serial=self.serial.checked_add(1).context("storage serial exhausted")?;
            let request=if release {Request::ReleaseSeed {slot:intent.slot,retirement:intent.id}}
                else {Request::RetireSeed {slot:intent.slot,retirement:intent.id,readers:intent.readers.clone()}};
            let bytes=service::encode(&Envelope {version:1,serial,constraint:intent.original.constraint(),request},&[])?;
            intent.packet=Some(RetirementPacket {serial,bytes,release,attempted:false,replayed:false});self.serial=serial;
        }
        // At most one send per invocation; only an explicit subsequent call
        // may replay an uncertain original packet once. No timed resend loop.
        let packet=intent.packet.as_ref().unwrap();
        ensure!(packet.release==release,"original retirement phase changed");
        let mut may_send=!packet.attempted||!packet.replayed;
        loop {
            self.current_for_retirement_intent(intent)?;intent.original.remaining()?;
            match self.receive_retirement_ack(intent)? {
                RetirementReply::Duplicate=>continue,
                RetirementReply::Accepted=>{self.current_for_retirement_intent(intent)?;intent.original.remaining()?;return Ok(());}
                RetirementReply::Partial=>anyhow::bail!("original Root cleanup partial; same owner/budget only"),
                RetirementReply::None=>{},
            }
            let packet=intent.packet.as_mut().unwrap();
            if may_send {
                let was_attempted=packet.attempted;
                if was_attempted {packet.replayed=true;}packet.attempted=true;may_send=false;
                if !self.endpoint.send(&packet.bytes,None)? {
                    packet.attempted=was_attempted;if was_attempted {packet.replayed=false;}may_send=true;
                }
            }
            std::thread::sleep(intent.original.phase_limit(Duration::from_millis(2))?);
        }
    }

    fn receive_retirement_ack(&self,intent:&mut ClientRetirement)->Result<RetirementReply> {
        let Some(packet)=self.endpoint.receive()? else {return Ok(RetirementReply::None);};
        ensure!(packet.fds.is_empty()&&packet.credentials.pid==self.server.pid as i32&&packet.credentials.uid==0&&packet.credentials.gid==0,"retirement reply is not original Root");
        let digest=sha2::Sha256::digest(&packet.bytes).to_vec();
        let (answer,payload):(Answer,_)=service::decode(&packet.bytes)?;
        if intent.last_reply.as_ref().is_some_and(|(serial,old)|*serial==answer.serial&&old==&digest) {return Ok(RetirementReply::Duplicate);}
        let original=intent.packet.as_ref().context("original immutable retirement packet absent")?;
        ensure!(answer.version==1&&payload.is_empty()&&answer.serial==original.serial,"original retirement ACK serial differs");
        let state=match answer.reply {
            Reply::SeedRetired {slot,retirement} if !original.release&&slot==intent.slot&&retirement==intent.id=>{
                intent.ack=Some(RootSeedRetired {_instance:self.instance.context("original service absent")?,_slot:slot,_operation:retirement});RetirementReply::Accepted
            }
            Reply::SeedReleased {slot,retirement} if original.release&&slot==intent.slot&&retirement==intent.id=>{intent.released=true;RetirementReply::Accepted}
            Reply::RetirementPending {slot,retirement} if slot==intent.slot&&retirement==intent.id=>RetirementReply::Partial,
            _=>anyhow::bail!("original retirement ACK invalid; owners retained"),
        };
        // Genuine original ACK retained BEFORE every fallible postcheck.
        intent.last_reply=Some((answer.serial,digest));intent.packet=None;Ok(state)
    }
    /// OFF App, at most ONE nonblocking receive. Even after expiry, an actual
    /// queued original ACK can establish a past fact. No send, unpin, cleanup,
    /// new budget, readback promotion, or automatic continuation occurs.
    pub fn reconcile_retirement_ack_retaining(&mut self,owner:&mut ClientSeedOwner)->Result<bool> {
        self.current_for_retained_cleanup(owner)?;let intent=owner.retirement.as_mut().context("original retirement absent")?;
        ensure!(self.retirement_inflight==Some(intent.id),"another original retirement owns the channel");
        if intent.packet.is_some() {let _=self.receive_retirement_ack(intent)?;}
        self.current_for_retirement_intent(intent)?;
        if intent.released {self.retirement_inflight=None;Ok(true)} else {Ok(false)}
    }

}

#[path="root_storage_paged_client.rs"]
mod paged_client;
pub use paged_client::{ClientMetadataCowOwner,ArchivedMetadataCow};

#[path="root_storage_seed_window.rs"]
mod seed_window;

#[derive(Clone,Copy)]
enum RootPrivateEffect {SeedPreparation,CowPreparation,CowEnrollment,CowArchive}
/// Refusal/continuity witness only. It permits old App/PTY/mail polling while
/// NEW Root IO remains closed; it grants no task, lease, source or IPC effect.
pub struct RetainedRootPrivateIoCustody<'a>{_client:&'a RootStorageClient}
impl RootStorageClient {
    pub fn retain_original_v2_private_io_custody(&self)->Result<RetainedRootPrivateIoCustody<'_>>{
        self.current_physical()?;ensure!(self.instance.is_some()&&self.private_v2_recovery.is_some(),"Root error is not a retained V2 private effect");
        Ok(RetainedRootPrivateIoCustody{_client:self})
    }
}

struct GenericOriginal {serial:u64,bytes:Vec<u8>,fd:Option<OwnedFd>,packet:Option<service::Packet>}

impl RootStorageClient {
    /// Physical custody for fixed original-resource cleanup only. This never
    /// clears fenced, accepts a command or renews project/task permission.
    pub fn assert_retained_seed_cleanup_custody(&self,owner:&ClientSeedOwner)->Result<()>{self.current_for_retained_cleanup(owner)}
    fn current_for_retained_cleanup(&self,owner:&ClientSeedOwner)->Result<()> {
        if !self.fenced{return self.current()}
        ensure!(owner.profile==rudder_vcs_bridge::seed::SeedProfile::PagedCowV2&&self.private_v2_recovery.is_some()
            &&self.generic_inflight.is_none()&&self.seed_window_inflight.is_none()&&self.cow_inflight.is_none(),"original Root messages still unknown; cleanup carrier retained");
        self.current_physical()?;self.original_slot(owner)?;Ok(())
    }
    fn current_for_retirement_intent(&self,intent:&ClientRetirement)->Result<()> {
        if !self.fenced{return self.current()}
        ensure!(intent.paged&&self.private_v2_recovery.is_some()&&self.generic_inflight.is_none()&&self.seed_window_inflight.is_none()&&self.cow_inflight.is_none(),"Root retirement cannot interleave an unknown original message");
        self.current_physical()
    }
}
