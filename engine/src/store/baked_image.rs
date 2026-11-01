//! Bounded immutable image byte DATA, not LocalStore/GC/root/task authority.
//! Only a genuine separately authenticated image owner may later admit this
//! backing. Saved inventory/pin JSON or this reader cannot issue that owner.
//! No path fallback, writes, workers, automatic cleanup or filesystem IO in Drop.
use super::*;
use std::{ffi::CString, os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::FileExt as ReadAt, sync::{Arc, Mutex, MutexGuard, atomic::{AtomicUsize, Ordering}}};

pub const BAKED_IMAGE_DIRECTORY: &str = "/var/lib/rudder-image-cas-v1";
pub const BAKED_IMAGE_LAYOUT: &str = "baked-image-cas-v1";
pub const MAX_BAKED_OBJECTS: usize = 65_536;
pub const MAX_BAKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_BAKED_INVENTORY_BYTES: u64 = 16 * 1024 * 1024;
const CHUNK: u64 = 4 * 1024 * 1024;
const MANIFEST: u64 = 32 * 1024 * 1024;
const MAX_READS: usize = 8;
const MAX_READ_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BakedImageRoots { pub format_version: u32, pub manifest: ObjectRef, pub installer_input: ObjectRef }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BakedImageInventoryData {
    pub format_version: u32, pub layout: String, pub roots: BakedImageRoots,
    pub objects: Vec<ObjectRef>, pub total_bytes: u64, pub retention: ObjectRef,
    pub retention_generation: u64, pub pins: BTreeMap<String, ObjectRef>,
}
impl BakedImageInventoryData {
    /// Shape/quotas only. This function creates no admission/complete pin proof.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.format_version == 1 && self.layout == BAKED_IMAGE_LAYOUT && self.roots.format_version == 1,
            "unsupported baked image DATA version");
        ensure!(self.roots.installer_input.kind == ObjectKind::Directory && self.roots.manifest.kind == ObjectKind::Blob,
            "baked image root kinds differ");
        bound(&self.roots.installer_input, false)?; bound(&self.roots.manifest, true)?;
        ensure!(!self.objects.is_empty() && self.objects.len() <= MAX_BAKED_OBJECTS
            && self.objects.windows(2).all(|w| w[0] < w[1]), "baked image inventory bound/order");
        let mut bytes = 0u64;
        for reference in &self.objects {
            bound(reference, *reference == self.roots.manifest)?;
            bytes = bytes.checked_add(reference.size_bytes).context("baked inventory size overflow")?;
        }
        ensure!(bytes == self.total_bytes && bytes <= MAX_BAKED_BYTES
            && self.objects.binary_search(&self.roots.manifest).is_ok()
            && self.objects.binary_search(&self.roots.installer_input).is_ok(), "baked image inventory roots/bytes differ");
        ensure!(self.pins.len() == 2 && self.pins.values().any(|r| r == &self.roots.manifest)
            && self.pins.values().any(|r| r == &self.roots.installer_input), "baked image DATA needs its two original pins");
        for (name, reference) in &self.pins { validate_pin(name)?; reference.validate()?; }
        self.retention.validate()?;
        ensure!(self.retention.kind == ObjectKind::Blob && self.retention.size_bytes <= MAX_BAKED_INVENTORY_BYTES,
            "baked retention DATA bound"); Ok(())
    }
}
fn bound(reference: &ObjectRef, manifest: bool) -> Result<()> {
    reference.validate()?;
    let maximum = match reference.kind {
        ObjectKind::Blob => if manifest { MANIFEST } else { CHUNK },
        ObjectKind::Directory | ObjectKind::DirectoryShard | ObjectKind::File => 64 * 1024,
        _ => bail!("object kind outside baked image content profile"),
    };
    ensure!(reference.size_bytes <= maximum && (reference.kind == ObjectKind::Blob || reference.size_bytes > 0)
        && (!manifest || reference.kind == ObjectKind::Blob && reference.size_bytes > 0), "baked object exceeds byte profile"); Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp { dev: u64, ino: u64, len: u64, mode: u32, uid: u32, gid: u32, links: u64,
    mtime: (i64, i64), ctime: (i64, i64) }
fn stamp(file: &File) -> Result<Stamp> {
    let m = file.metadata()?;
    Ok(Stamp { dev:m.dev(), ino:m.ino(), len:m.len(), mode:m.mode(), uid:m.uid(), gid:m.gid(), links:m.nlink(),
        mtime:(m.mtime(),m.mtime_nsec()), ctime:(m.ctime(),m.ctime_nsec()) })
}
fn descriptor(file: &File) -> Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    let cloexec = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    ensure!(flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY
        && cloexec >= 0 && cloexec & libc::FD_CLOEXEC != 0, "baked descriptor must be original readonly/CLOEXEC"); Ok(())
}
fn directory(file: &File, owner: (u32,u32)) -> Result<Stamp> {
    descriptor(file)?; let s = stamp(file)?;
    ensure!(s.mode & libc::S_IFMT == libc::S_IFDIR && s.mode & 0o7777 == 0o555
        && (s.uid,s.gid) == owner, "baked directory ownership/type/mode invalid"); Ok(s)
}
fn regular(file: &File, owner: (u32,u32), maximum: u64) -> Result<Stamp> {
    descriptor(file)?; let s = stamp(file)?;
    ensure!(s.mode & libc::S_IFMT == libc::S_IFREG && s.mode & 0o7777 == 0o444 && s.links == 1
        && (s.uid,s.gid) == owner && s.len <= maximum, "baked file ownership/type/mode/size invalid"); Ok(s)
}
fn open_at(parent: &File, name: &str, dir: bool) -> Result<File> {
    ensure!(!name.is_empty() && !name.contains(['/', '\0']) && name != "." && name != "..", "invalid baked child component");
    let name = CString::new(name)?;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW
        | libc::O_CLOEXEC | libc::O_NONBLOCK | if dir { libc::O_DIRECTORY } else { 0 }) };
    if fd < 0 { return Err(std::io::Error::last_os_error()).context("baked original child open failed"); }
    Ok(unsafe { File::from_raw_fd(fd) })
}
struct Directory { file: File, original: Option<Stamp> }
impl Directory {
    fn staged(file: File) -> Self { Self { file, original: None } }
    fn capture(&mut self, parent: &File, name: &str, owner: (u32,u32)) -> Result<()> {
        self.original = Some(directory(&self.file, owner)?); self.current(parent,name)
    }
    fn current(&self, parent: &File, name: &str) -> Result<()> {
        let expected = self.original.as_ref().context("baked directory capture incomplete")?;
        ensure!(stamp(&self.file)? == *expected && stamp(&open_at(parent,name,true)?)? == *expected,
            "baked original directory replaced/changed"); Ok(())
    }
}
struct State {
    root: Option<Stamp>, inventory_stamp: Option<Stamp>, cas: Option<Directory>, objects: Option<Directory>,
    shards: BTreeMap<String, Directory>, inventory: Option<BakedImageInventoryData>,
    pending: Option<File>, failed: bool, captured: bool,
}
/// Actual owned readonly FD source, DATA only. No image-policy, Root peer,
/// source, complete LocalStore pin, mount, task or execution grant is issued.
/// Future admission must retain its independently authenticated image owner.
pub struct BakedImageCas {
    root: File, inventory_file: File, expected_inventory: ObjectRef, owner: (u32,u32),
    state: Mutex<State>, active: AtomicUsize, bytes: AtomicUsize,
}
impl BakedImageCas {
    /// Stage original descriptors before any reads/checks. Errors retain this
    /// same output and every partially opened directory/read FD. No path API.
    pub fn capture_retaining(out: &mut Option<Arc<Self>>, root: OwnedFd, inventory: OwnedFd,
        expected_inventory: ObjectRef, original: Arc<dyn ImageImportFence>) -> Result<()> {
        Self::capture_owned(out,root,inventory,expected_inventory,original,(0,0))
    }
    /// Ordinary-UID real-file component fixtures only. Compiled out of every
    /// production artifact; this cannot qualify Root/image admission.
    #[cfg(test)]
    pub(crate) fn capture_local_fixture_retaining(out:&mut Option<Arc<Self>>,root:OwnedFd,inventory:OwnedFd,
        expected_inventory:ObjectRef,original:Arc<dyn ImageImportFence>)->Result<()> {
        Self::capture_owned(out,root,inventory,expected_inventory,original,
            (unsafe{libc::geteuid()},unsafe{libc::getegid()}))
    }
    fn capture_owned(out: &mut Option<Arc<Self>>, root: OwnedFd, inventory: OwnedFd,
        expected_inventory: ObjectRef, original: Arc<dyn ImageImportFence>, owner: (u32,u32)) -> Result<()> {
        ensure!(out.is_none(), "original baked source carrier occupied");
        *out = Some(Arc::new(Self { root:File::from(root), inventory_file:File::from(inventory), expected_inventory, owner,
            state:Mutex::new(State {root:None,inventory_stamp:None,cas:None,objects:None,shards:BTreeMap::new(),
                inventory:None,pending:None,failed:false,captured:false}), active:AtomicUsize::new(0),bytes:AtomicUsize::new(0) }));
        let source=out.as_ref().unwrap(); let mut state=source.lock(&*original)?;
        let result=source.capture(&mut state,&*original);
        if result.is_err(){state.failed=true;} result
    }
    fn lock(&self, original: &dyn ImageImportFence) -> Result<MutexGuard<'_,State>> {
        loop { original.remaining()?;
            match self.state.try_lock() {
                Ok(guard)=>return Ok(guard),
                Err(std::sync::TryLockError::WouldBlock)=>std::thread::sleep(original.remaining()?.min(Duration::from_millis(1))),
                Err(_)=>bail!("baked source custody poisoned; retain original owner"),
            }
        }
    }
    fn check(&self,state:&State,shard:Option<&str>,original:&dyn ImageImportFence)->Result<()> {
        original.remaining()?; ensure!(!state.failed,"baked source fenced; original owner retained");
        ensure!(state.root.as_ref()==Some(&stamp(&self.root)?),"original baked root changed");
        if let Some(cas)=&state.cas{cas.current(&self.root,"cas")?;}
        if let (Some(cas),Some(objects))=(&state.cas,&state.objects){objects.current(&cas.file,"objects")?;}
        if let Some(name)=shard{state.shards.get(name).context("original baked shard absent")?
            .current(&state.objects.as_ref().context("original objects absent")?.file,name)?;}
        if let Some(expected)=&state.inventory_stamp {
            ensure!(stamp(&self.inventory_file)?==*expected && stamp(&open_at(&self.root,"inventory.json",false)?)?==*expected,
                "original baked inventory descriptor changed");
        }
        original.remaining()?;Ok(())
    }
    fn capture(&self,state:&mut State,original:&dyn ImageImportFence)->Result<()> {
        original.remaining()?; self.expected_inventory.validate()?;
        ensure!(self.expected_inventory.kind==ObjectKind::Blob && self.expected_inventory.size_bytes<=MAX_BAKED_INVENTORY_BYTES,"baked inventory reference bound");
        state.root=Some(directory(&self.root,self.owner)?);
        state.inventory_stamp=Some(regular(&self.inventory_file,self.owner,MAX_BAKED_INVENTORY_BYTES)?);
        self.check(state,None,original)?;
        let raw=read_full(&self.inventory_file,&self.expected_inventory,MAX_BAKED_INVENTORY_BYTES,original)?;
        let data:BakedImageInventoryData=serde_json::from_slice(&raw)?;
        ensure!(canonical_json(&data)?==raw,"baked inventory encoding not canonical");data.validate()?;
        state.inventory=Some(data);
        original.remaining()?;
        state.cas=Some(Directory::staged(open_at(&self.root,"cas",true)?));
        state.cas.as_mut().unwrap().capture(&self.root,"cas",self.owner)?;
        state.objects=Some(Directory::staged(open_at(&state.cas.as_ref().unwrap().file,"objects",true)?));
        state.objects.as_mut().unwrap().capture(&state.cas.as_ref().unwrap().file,"objects",self.owner)?;
        self.check(state,None,original)?;
        // The immutable retention snapshot is checked as DATA. It cannot be
        // adopted as LocalStore authority or recreate an original pin ACK.
        let data=state.inventory.as_ref().unwrap().clone();
        original.remaining()?;
        state.pending=Some(open_at(&state.cas.as_ref().unwrap().file,"retention.json",false)?);
        regular(state.pending.as_ref().unwrap(),self.owner,MAX_BAKED_INVENTORY_BYTES)?;
        let raw=read_full(state.pending.as_ref().unwrap(),&data.retention,MAX_BAKED_INVENTORY_BYTES,original)?;
        let envelope:StateEnvelope=serde_json::from_slice(&raw)?;
        ensure!(ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&envelope.state)?).digest==envelope.checksum
            && envelope.state.format_version==1 && envelope.state.generation==data.retention_generation
            && envelope.state.pins==data.pins && envelope.state.staging.is_empty() && envelope.state.image_import_holds.is_empty(),
            "baked retention DATA differs from original inventory");
        state.pending.take();
        original.remaining()?;state.pending=Some(open_at(&self.root,"pin.json",false)?);let pin_stamp=regular(state.pending.as_ref().unwrap(),self.owner,4096)?;
        let mut pin=vec![0;usize::try_from(pin_stamp.len)?];read_exact(state.pending.as_ref().unwrap(),&mut pin,original)?;
        ensure!(pin==canonical_json(&data.roots)? && stamp(state.pending.as_ref().unwrap())?==pin_stamp,"baked pin DATA differs");
        state.pending.take();
        self.check(state,None,original)?;state.captured=true;Ok(())
    }
    pub fn inventory_data(&self)->Result<BakedImageInventoryData>{
        let state=self.state.try_lock().map_err(|_|anyhow::anyhow!("baked source busy/poisoned"))?;
        ensure!(state.captured&&!state.failed,"baked inventory capture incomplete");Ok(state.inventory.as_ref().unwrap().clone())
    }
    pub fn read_object_retaining(self:&Arc<Self>,out:&mut Option<BakedImageRead>,reference:ObjectRef,
        original:Arc<dyn ImageImportFence>)->Result<()> {
        self.read_retaining(out,reference,false,original)
    }
    /// Only this distinct operation admits the separate <=32MiB manifest blob.
    /// It cannot enlarge ordinary runtime chunks or return an executable FD.
    pub fn read_manifest_retaining(self:&Arc<Self>,out:&mut Option<BakedImageRead>,original:Arc<dyn ImageImportFence>)->Result<()> {
        let reference={let state=self.lock(&*original)?;ensure!(state.captured&&!state.failed,"baked manifest capture incomplete");state.inventory.as_ref().unwrap().roots.manifest.clone()};
        self.read_retaining(out,reference,true,original)
    }
    fn read_retaining(self:&Arc<Self>,out:&mut Option<BakedImageRead>,reference:ObjectRef,manifest:bool,
        original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original baked read output occupied");
        *out=Some(BakedImageRead::stage(self.clone(),reference,manifest,original));out.as_mut().unwrap().run()
    }
}
fn read_exact(file:&File,bytes:&mut[u8],original:&dyn ImageImportFence)->Result<()> {
    let mut offset=0usize;
    while offset<bytes.len(){original.remaining()?;let count=(bytes.len()-offset).min(64*1024);
        let n=file.read_at(&mut bytes[offset..offset+count],offset as u64)?;ensure!(n>0,"baked object ended early");offset+=n;}
    let mut extra=[0];original.remaining()?;ensure!(file.read_at(&mut extra,bytes.len()as u64)?==0,"baked object grew");original.remaining()?;Ok(())
}
fn read_full(file:&File,reference:&ObjectRef,maximum:u64,original:&dyn ImageImportFence)->Result<Vec<u8>> {
    reference.validate()?;ensure!(reference.size_bytes<=maximum,"baked metadata bound");let before=stamp(file)?;
    ensure!(before.len==reference.size_bytes,"baked metadata length differs");let mut bytes=vec![0;usize::try_from(reference.size_bytes)?];
    read_exact(file,&mut bytes,original)?;reference.verify(&bytes)?;
    original.remaining()?;ensure!(stamp(file)?==before,"baked metadata changed during read");original.remaining()?;Ok(bytes)
}
struct Reservation {source:Arc<BakedImageCas>,active:bool,bytes:usize}
impl Drop for Reservation {fn drop(&mut self){if self.active{self.source.active.fetch_sub(1,Ordering::AcqRel);}if self.bytes>0{self.source.bytes.fetch_sub(self.bytes,Ordering::AcqRel);}}}
/// Original synchronous read custody. Partial/unverified bytes stay private;
/// errors/panics keep actual FD, source directories and memory reservation.
pub struct BakedImageRead {source:Arc<BakedImageCas>,reference:ObjectRef,manifest:bool,original:Arc<dyn ImageImportFence>,
    file:Option<File>,original_stamp:Option<Stamp>,bytes:Vec<u8>,reservation:Option<Reservation>,attempted:bool,verified:bool,
    #[cfg(test)] panic_after_first_chunk:bool,#[cfg(test)]cancel_after_first_chunk:Option<Arc<std::sync::atomic::AtomicBool>>}
impl BakedImageRead {
    fn stage(source:Arc<BakedImageCas>,reference:ObjectRef,manifest:bool,original:Arc<dyn ImageImportFence>)->Self {
        Self{source,reference,manifest,original,file:None,original_stamp:None,bytes:Vec::new(),reservation:None,attempted:false,verified:false,
            #[cfg(test)]panic_after_first_chunk:false,#[cfg(test)]cancel_after_first_chunk:None}
    }
    pub fn reference_data(&self)->&ObjectRef{&self.reference}
    pub fn verified_bytes(&self)->Result<&[u8]>{ensure!(self.verified,"original baked bytes unverified/unknown");Ok(&self.bytes)}
    fn run(&mut self)->Result<()> {
        ensure!(!self.attempted,"original baked read cannot retry");self.attempted=true;self.original.remaining()?;bound(&self.reference,self.manifest)?;
        let source=self.source.clone();let mut state=source.lock(&*self.original)?;
        ensure!(state.captured&&!state.failed,"baked source not captured/current");
        let data=state.inventory.as_ref().unwrap();
        ensure!(data.objects.binary_search(&self.reference).is_ok()&&(!self.manifest||self.reference==data.roots.manifest),"baked typed reference outside captured inventory");
        self.reservation=Some(Reservation{source:source.clone(),active:false,bytes:0});
        source.active.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|(n<MAX_READS).then_some(n+1)).map_err(|_|anyhow::anyhow!("baked read owners full"))?;
        self.reservation.as_mut().unwrap().active=true;
        let amount=usize::try_from(self.reference.size_bytes)?;
        source.bytes.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|n.checked_add(amount).filter(|v|*v<=MAX_READ_BYTES)).map_err(|_|anyhow::anyhow!("baked read byte reservations full"))?;
        self.reservation.as_mut().unwrap().bytes=amount;
        let result=self.read_inner(&mut state);
        // A particular reader's deadline/cancellation does not revoke immutable
        // physical custody used by another legitimate operation. The failed
        // read stays nonretryable and retains its own FD/reservation/bytes.
        if result.is_err()&&self.original.remaining().is_ok(){state.failed=true;}result
    }
    fn read_inner(&mut self,state:&mut State)->Result<()> {
        let shard=self.reference.digest[..2].to_owned();let leaf=self.reference.digest[2..].to_owned();
        self.source.check(state,None,&*self.original)?;
        if !state.shards.contains_key(&shard){ensure!(state.shards.len()<256,"baked shard FD bound");
            state.shards.insert(shard.clone(),Directory::staged(open_at(&state.objects.as_ref().unwrap().file,&shard,true)?));
            state.shards.get_mut(&shard).unwrap().capture(&state.objects.as_ref().unwrap().file,&shard,self.source.owner)?;}
        self.source.check(state,Some(&shard),&*self.original)?;
        self.file=Some(open_at(&state.shards[&shard].file,&leaf,false)?);
        self.original_stamp=Some(regular(self.file.as_ref().unwrap(),self.source.owner,if self.manifest{MANIFEST}else{CHUNK})?);
        let expected=self.original_stamp.as_ref().unwrap();ensure!(expected.len==self.reference.size_bytes,"baked object size mismatch");
        ensure!(stamp(&open_at(&state.shards[&shard].file,&leaf,false)?)?==*expected,"baked object name changed at open");
        self.bytes.reserve_exact(usize::try_from(expected.len)?);
        let mut buffer=[0;64*1024];let mut sha=Sha256::new();let mut offset=0u64;
        loop {self.original.remaining()?;let n=self.file.as_ref().unwrap().read_at(&mut buffer,offset)?;if n==0{break;}
            ensure!(offset+(n as u64)<=self.reference.size_bytes,"baked object grew during verification");
            sha.update(&buffer[..n]);self.bytes.extend_from_slice(&buffer[..n]);offset+=n as u64;
            #[cfg(test)] if let Some(flag)=&self.cancel_after_first_chunk{flag.store(true,Ordering::Release);}
            #[cfg(test)] if self.panic_after_first_chunk{panic!("fixture original read panic");}
        }
        // Compute typed identity; metadata parsing/edges are independent of raw
        // SHA and never assert a complete root or actual child availability.
        let actual=ObjectRef{kind:self.reference.kind,algorithm:crate::protocol::HashAlgorithm::Sha256,
            digest:hex::encode(sha.finalize()),size_bytes:offset};
        ensure!(actual==self.reference,"baked typed content checksum/size mismatch");
        references_from_bytes(self.reference.kind,&self.bytes)?;
        ensure!(stamp(self.file.as_ref().unwrap())?==*self.original_stamp.as_ref().unwrap()
            &&stamp(&open_at(&state.shards[&shard].file,&leaf,false)?)?==*self.original_stamp.as_ref().unwrap(),"baked object replaced/changed during read");
        self.source.check(state,Some(&shard),&*self.original)?;self.original.remaining()?;self.verified=true;Ok(())
    }
}

#[cfg(test)]
#[path="baked_image_tests.rs"]
mod tests;
