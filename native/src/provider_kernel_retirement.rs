//! Explicit OFF-App disposal of this probe's original private fixture only.
//! A failed effect is retained, never replayed or inferred from path absence.
use super::*;
use fs2::FileExt as LockExt;
use std::{ffi::CString, os::unix::ffi::OsStrExt};

const MAX_ENTRIES: usize = 512;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
#[derive(Clone, Copy, PartialEq, Eq)]
struct Id { dev: u64, ino: u64, kind: u32, uid: u32, gid: u32, mode: u32 }
fn id(m: &fs::Metadata) -> Id { Id { dev:m.dev(), ino:m.ino(), kind:m.mode() & libc::S_IFMT, uid:m.uid(), gid:m.gid(), mode:m.mode() & 0o7777 } }
fn named(parent: &File, name: &[u8]) -> Result<libc::stat> {
    let name = CString::new(name)?;
    let mut raw = std::mem::MaybeUninit::uninit();
    ensure!(unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), raw.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } == 0, "original fixture name unavailable");
    Ok(unsafe { raw.assume_init() })
}
fn same_named(parent: &File, name: &[u8], file: &File) -> Result<()> {
    let n = named(parent,name)?; let h = file.metadata()?;
    ensure!((n.st_dev,n.st_ino,n.st_mode,n.st_uid,n.st_gid) == (h.dev(),h.ino(),h.mode(),h.uid(),h.gid()), "original fixture entry replaced"); Ok(())
}
fn open_beneath(root: &File, path: &[u8], directory: bool) -> Result<File> {
    #[repr(C)] struct How { flags: u64, mode: u64, resolve: u64 }
    let path = CString::new(path)?;
    let flags = libc::O_CLOEXEC | libc::O_NOFOLLOW | if directory { libc::O_RDONLY | libc::O_DIRECTORY } else { libc::O_PATH };
    // NO_XDEV/NO_MAGICLINKS/NO_SYMLINKS/BENEATH; bind mounts are refused too.
    let how = How { flags:flags as u64, mode:0, resolve:0x0f };
    let fd = unsafe { libc::syscall(libc::SYS_openat2, root.as_raw_fd(), path.as_ptr(), &how, std::mem::size_of::<How>()) };
    ensure!(fd >= 0, "anchored fixture open unavailable"); Ok(unsafe { File::from_raw_fd(fd as i32) })
}
fn names(dir: &File) -> Result<Vec<Vec<u8>>> {
    struct Reader(*mut libc::DIR);
    impl Drop for Reader { fn drop(&mut self) { unsafe { libc::closedir(self.0); } } }
    // Reopen '.', never dup a shared readdir offset.
    let fd = open_beneath(dir,b".",true)?;
    let raw = std::os::fd::IntoRawFd::into_raw_fd(fd);
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() { unsafe { libc::close(raw); } return Err(fault()); }
    let reader = Reader(stream); let mut out = Vec::new();
    loop {
        unsafe { *libc::__errno_location() = 0; }
        let entry = unsafe { libc::readdir(reader.0) };
        if entry.is_null() { ensure!(std::io::Error::last_os_error().raw_os_error() == Some(0), "fixture readdir incomplete"); break; }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." { continue; }
        ensure!(!name.is_empty() && name.len() <= 255 && !name.contains(&b'/'), "unsafe fixture name");
        out.push(name.to_vec()); ensure!(out.len() <= MAX_ENTRIES, "fixture directory entry bound");
    }
    out.sort(); ensure!(out.windows(2).all(|w| w[0] != w[1]), "duplicate fixture directory name"); Ok(out)
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Role { Root, Projection }
pub(super) struct DirectoryAnchor { parent:File, file:File, name:Vec<u8>, identity:Id, parent_id:Id, role:Role }
impl DirectoryAnchor {
    fn capture(parent:File, file:File, name:Vec<u8>, role:Role) -> Result<Self> {
        // Caller stages the original handles before calling this validator.
        let m=file.metadata()?; let p=parent.metadata()?;
        ensure!(m.is_dir() && p.is_dir() && m.uid()==unsafe { libc::geteuid() } && m.gid()==unsafe { libc::getegid() } && m.mode() & 0o7777==0o700 && p.uid()==m.uid() && p.gid()==m.gid() && p.mode() & 0o7777==0o700, "fixture directories not private");
        let value=Self { parent,file,name,identity:id(&m),parent_id:id(&p),role }; value.check()?; Ok(value)
    }
    fn check(&self) -> Result<()> {
        ensure!(id(&self.file.metadata()?)==self.identity && id(&self.parent.metadata()?)==self.parent_id, "fixture directory custody changed");
        same_named(&self.parent,&self.name,&self.file)?;
        ensure!(id(&open_beneath(&self.parent,&self.name,true)?.metadata()?)==self.identity, "fixture directory is a mount/replacement"); Ok(())
    }
}
pub(super) struct FileAnchor { parent:File, file:File, name:Vec<u8>, identity:Stamp }
impl FileAnchor {
    fn capture(parent:File, file:File, name:Vec<u8>) -> Result<Self> {
        let m=file.metadata()?;
        ensure!(m.is_file() && m.uid()==unsafe { libc::geteuid() } && m.gid()==unsafe { libc::getegid() } && m.nlink()==1 && m.mode() & 0o7777==0o600, "fixture file not private single-link");
        let value=Self { parent,file,name,identity:stamp(&m) }; value.check()?; Ok(value)
    }
    fn check(&self) -> Result<()> { ensure!(stamp(&self.file.metadata()?)==self.identity, "original fixture file changed"); same_named(&self.parent,&self.name,&self.file) }
    fn lock_original(&self) -> Result<File> {
        self.check()?; let name=CString::new(self.name.clone())?;
        let raw=unsafe { libc::openat(self.parent.as_raw_fd(),name.as_ptr(),libc::O_RDWR|libc::O_NOFOLLOW|libc::O_CLOEXEC) };
        ensure!(raw>=0,"original broker lock unavailable"); let file=unsafe { File::from_raw_fd(raw) };
        ensure!(stamp(&file.metadata()?)==self.identity,"original broker lock changed");
        file.try_lock_exclusive()?; Ok(file) // Retained until unlink/fsync is ACKed.
    }
}
pub(super) fn open_original_directory(path:&Path) -> Result<File> {
    let m=fs::symlink_metadata(path)?;
    ensure!(m.is_dir() && !m.file_type().is_symlink() && m.uid()==unsafe { libc::geteuid() } && m.gid()==unsafe { libc::getegid() } && m.mode() & 0o7777==0o700 && path.canonicalize()?==path,"fixture directory redirects");
    let f=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?;
    ensure!(id(&f.metadata()?)==id(&m),"fixture directory open changed"); Ok(f)
}
pub(super) fn require_absent(path:&Path) -> Result<()> {
    match fs::symlink_metadata(path) { Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(()), _=>anyhow::bail!("fixture projection/lock preexists; never adopt") }
}
/// Original handles are retained BEFORE validation; failure keeps them in State.
#[derive(Default)]
pub(super) struct Anchors { base:Option<File>, root:Option<DirectoryAnchor>, projection_parent:Option<File>, projection:Option<DirectoryAnchor>, identity:Option<FileAnchor>, broker_lock:Option<FileAnchor>, cas:Option<File>, vfs:Option<File>, view:Option<(File,Id)>, view_files:Vec<(Vec<u8>,File,Id)>, raw:Vec<File> }
impl Anchors {
    pub(super) fn capture_base(&mut self,path:&Path) -> Result<()> { self.base=Some(open_original_directory(path)?); Ok(()) }
    pub(super) fn capture_root(&mut self,root:&File,name:&std::ffi::OsStr) -> Result<()> {
        self.raw.push(root.try_clone()?);
        let parent=self.base.as_ref().context("original parent absent")?.try_clone()?;
        self.root=Some(DirectoryAnchor::capture(parent,root.try_clone()?,name.as_bytes().to_vec(),Role::Root)?); Ok(())
    }
    pub(super) fn capture_cas(&mut self,path:&Path) -> Result<()> { self.cas=Some(open_original_directory(path)?); Ok(()) }
    pub(super) fn capture_vfs(&mut self,path:&Path) -> Result<()> { self.vfs=Some(open_original_directory(path)?); Ok(()) }
    pub(super) fn capture_view(&mut self,view_id:&str)->Result<()> {
        let root=self.vfs.as_ref().context("original manager root absent")?;
        let path=format!("views/{view_id}");let file=open_beneath(root,path.as_bytes(),true)?;
        self.raw.push(file.try_clone()?);self.view=Some((file.try_clone()?,id(&file.metadata()?)));
        for name in [b"namespace.sqlite".as_slice(),b"owner.lock".as_slice()] {
            self.raw.push(open_beneath(&file,name,false)?);let held=self.raw.last().unwrap();let m=held.metadata()?;
            ensure!(m.is_file()&&m.uid()==unsafe{libc::geteuid()}&&m.gid()==unsafe{libc::getegid()}&&m.nlink()==1&&m.mode()&0o022==0,"original private view file unsafe");
            self.view_files.push((name.to_vec(),held.try_clone()?,id(&m)));
        }Ok(())
    }
    fn check_view(&self,view_id:&str)->Result<()> {
        let (held,identity)=self.view.as_ref().context("original view directory absent")?;
        let root=self.vfs.as_ref().context("original manager root absent")?;let path=format!("views/{view_id}");
        ensure!(id(&held.metadata()?)==*identity&&id(&open_beneath(root,path.as_bytes(),true)?.metadata()?)==*identity,"original view directory replaced/mounted");
        for (name,file,identity) in &self.view_files {
            ensure!(id(&file.metadata()?)==*identity&&file.metadata()?.nlink()==1,"original namespace/lock changed");
            same_named(held,name,file)?;ensure!(id(&open_beneath(held,name,false)?.metadata()?)==*identity,"original namespace/lock remapped");
        }Ok(())
    }
    pub(super) fn before_projection(&mut self,socket:&Path) -> Result<()> {
        let leaf=socket.parent().context("projection leaf absent")?;
        require_absent(leaf)?; require_absent(&leaf.with_extension("lock"))?;
        // The endpoint helper may create its private shared base. Its original
        // successful ACK is captured immediately below; never clean that base.
        Ok(())
    }
    pub(super) fn projection_ack(&mut self,socket:&Path) -> Result<()> {
        let leaf=socket.parent().context("projection leaf absent")?;
        self.projection_parent=Some(open_original_directory(leaf.parent().context("projection parent absent")?)?);
        let parent=self.projection_parent.as_ref().unwrap();
        self.raw.push(open_beneath(parent,leaf.file_name().context("projection name absent")?.as_bytes(),true)?);
        let file=self.raw.last().unwrap().try_clone()?;
        self.projection=Some(DirectoryAnchor::capture(parent.try_clone()?,file.try_clone()?,leaf.file_name().unwrap().as_bytes().to_vec(),Role::Projection)?);
        self.raw.push(open_beneath(&file,b"identity.json",false)?);
        self.identity=Some(FileAnchor::capture(self.projection.as_ref().unwrap().file.try_clone()?,self.raw.last().unwrap().try_clone()?,b"identity.json".to_vec())?); Ok(())
    }
    pub(super) fn broker_ack(&mut self) -> Result<()> {
        let p=self.projection.as_ref().context("original projection absent")?;
        let mut name=p.name.clone(); name.extend_from_slice(b".lock");
        self.raw.push(open_beneath(&p.parent,&name,false)?);
        self.broker_lock=Some(FileAnchor::capture(p.parent.try_clone()?,self.raw.last().unwrap().try_clone()?,name)?); Ok(())
    }
    fn check_roots(&self,s:&State) -> Result<()> {
        self.root.as_ref().context("original root anchor absent")?.check()?;
        self.projection.as_ref().context("original projection anchor absent")?.check()?;
        self.identity.as_ref().context("original identity anchor absent")?.check()?;
        self.broker_lock.as_ref().context("original lock anchor absent")?.check()?;
        let root=&self.root.as_ref().unwrap().file;
        for (name,held,path) in [(b"cas".as_slice(),self.cas.as_ref(),s.store.as_ref().map(LocalStore::root)),(b"vfs".as_slice(),self.vfs.as_ref(),s.manager.as_ref().map(VfsManager::root))] {
            let held=held.context("original fixture subdirectory absent")?; same_named(root,name,held)?;
            ensure!(id(&open_beneath(root,name,true)?.metadata()?)==id(&held.metadata()?),"fixture CAS/VFS mount changed");
            let path=path.context("original fixture resource absent")?;
            ensure!((fs::metadata(path)?.dev(),fs::metadata(path)?.ino())==(held.metadata()?.dev(),held.metadata()?.ino()),"fixture CAS/VFS path changed");
        } self.check_view(&s.spec.as_ref().context("original spec absent")?.view_id)?; Ok(())
    }
}
struct Entry { file:File, parent:Option<usize>, name:Vec<u8>, identity:Stamp, dir:bool, attempted:bool, removed:bool }
struct Tree { entries:Vec<Entry>, next:usize, root_attempted:bool, root_removed:bool, synced:bool, #[cfg(test)] fail_parent_sync:bool }
impl Tree {
    fn capture(anchor:&DirectoryAnchor,original:&FinalizationBudget,cancel:&AtomicBool) -> Result<Self> {
        anchor.check()?;
        let mut out=Self { entries:Vec::new(),next:0,root_attempted:false,root_removed:false,synced:false,#[cfg(test)] fail_parent_sync:false };
        let mut pending:Vec<(Option<usize>,Vec<u8>,usize)>=vec![(None,Vec::new(),0)]; let mut bytes=0u64;
        while let Some((parent,path,depth))=pending.pop() {
            time(original,cancel)?; ensure!(depth<=32,"fixture cleanup depth bound");
            let directory=match parent { None=>anchor.file.try_clone()?,Some(i)=>out.entries[i].file.try_clone()? };
            for name in names(&directory)?.into_iter().rev() {
                time(original,cancel)?;ensure!(out.entries.len()<MAX_ENTRIES,"fixture cleanup entry bound");
                let mut relative=path.clone(); if !relative.is_empty() { relative.push(b'/'); } relative.extend_from_slice(&name);
                let raw=named(&directory,&name)?; let dir=raw.st_mode & libc::S_IFMT==libc::S_IFDIR;
                let file=open_beneath(&anchor.file,&relative,dir)?; same_named(&directory,&name,&file)?; let m=file.metadata()?;
                ensure!(m.uid()==anchor.identity.uid && m.gid()==anchor.identity.gid && m.mode() & 0o022==0,"foreign/mutable fixture entry");
                let regular=m.is_file(); let socket=m.mode() & libc::S_IFMT==libc::S_IFSOCK;
                let own_socket=socket && if anchor.role==Role::Projection { name==b"broker.sock" } else { relative.starts_with(b"workspace-rpc/") && !relative[b"workspace-rpc/".len()..].contains(&b'/') };
                ensure!(dir||regular||own_socket,"fixture cleanup refuses symlink/special/foreign socket");
                if !dir { ensure!(m.nlink()==1,"fixture cleanup refuses hardlink aliases"); bytes=bytes.checked_add(m.len()).context("fixture byte overflow")?; ensure!(bytes<=MAX_BYTES,"fixture cleanup byte bound"); }
                if anchor.role==Role::Projection { ensure!((name==b"identity.json"||name==b"broker.sock") && !dir,"foreign projection entry"); }
                let index=out.entries.len(); out.entries.push(Entry { file,parent,name,identity:stamp(&m),dir,attempted:false,removed:false });
                if dir { pending.push((Some(index),relative,depth+1)); }
            }
        }
        out.next=out.entries.len(); Ok(out)
    }
    fn remove(&mut self,anchor:&DirectoryAnchor,original:&FinalizationBudget,cancel:&AtomicBool) -> Result<()> {
        anchor.check()?;
        while self.next>0 {
            time(original,cancel)?; let i=self.next-1; let e=&self.entries[i];
            ensure!(!e.attempted,"uncertain fixture unlink cannot replay");
            let parent=match e.parent { Some(p)=>self.entries[p].file.try_clone()?,None=>anchor.file.try_clone()? };
            same_named(&parent,&e.name,&e.file)?; let m=e.file.metadata()?;
            ensure!(if e.dir { id(&m)==id_from_stamp(&e.identity) } else { stamp(&m)==e.identity },"fixture entry changed before unlink");
            let name=CString::new(e.name.clone())?; let flags=if e.dir { libc::AT_REMOVEDIR } else { 0 };
            self.entries[i].attempted=true; // Staged before the syscall; failure is absorbing.
            ensure!(unsafe { libc::unlinkat(parent.as_raw_fd(),name.as_ptr(),flags) }==0,"fixture unlink uncertain");
            self.entries[i].removed=true; self.next=i; // Actual ACK retained before fsync/checks.
            parent.sync_all()?;
        }
        ensure!(names(&anchor.file)?.is_empty(),"fixture gained unknown entries"); same_named(&anchor.parent,&anchor.name,&anchor.file)?;
        ensure!(!self.root_attempted,"uncertain root unlink cannot replay"); time(original,cancel)?;
        let name=CString::new(anchor.name.clone())?; self.root_attempted=true;
        ensure!(unsafe { libc::unlinkat(anchor.parent.as_raw_fd(),name.as_ptr(),libc::AT_REMOVEDIR) }==0,"fixture root unlink uncertain"); self.root_removed=true;
        #[cfg(test)] if self.fail_parent_sync { return Err(anyhow::anyhow!("injected parent fsync failure after actual root unlink")); }
        anchor.parent.sync_all()?; ensure!(anchor.file.metadata()?.nlink()==0,"fixture root still linked"); self.synced=true; time(original,cancel)
    }
}
fn id_from_stamp(s:&Stamp)->Id { Id { dev:s.dev,ino:s.ino,kind:s.mode & libc::S_IFMT,uid:s.uid,gid:s.gid,mode:s.mode & 0o7777 } }
/// Actual ACK of disposal of ONE original probe's fixtures. No public constructor
/// or Deserialize; aliases retain the same ACK and grant no startup authority.
#[derive(Clone)]
pub(crate) struct KernelProbeFixtureRetired { ack:Arc<Ack> }
struct Ack { nonce:String,view:String,root:(u64,u64) }
impl KernelProbeFixtureRetired {
    pub(crate) fn probe_nonce(&self)->&str { &self.ack.nonce }
    pub(crate) fn view_id(&self)->&str { &self.ack.view }
    pub(crate) fn original_root_identity(&self)->(u64,u64) { self.ack.root }
}
pub(super) struct Retirement {
    budget:FinalizationBudget, unknown:bool, panic:Option<Box<dyn std::any::Any+Send>>, error:Option<anyhow::Error>,
    view:Option<rudder_engine::vfs::VfsView>, destroyed:Option<rudder_engine::vfs::VfsCheckpoint>,
    pins:Vec<(String,rudder_engine::protocol::ObjectRef)>, unpinned:usize, stage_removed:bool,
    done:Option<KernelProbeFixtureRetired>, projection_tree:Option<Tree>, root_tree:Option<Tree>,
    resources_closed:bool, lock:Option<File>, lock_attempted:bool, lock_removed:bool,
}
impl Retirement {
    fn new(budget:FinalizationBudget)->Self { Self { budget,unknown:false,panic:None,error:None,view:None,destroyed:None,pins:Vec::new(),unpinned:0,stage_removed:false,done:None,projection_tree:None,root_tree:None,resources_closed:false,lock:None,lock_attempted:false,lock_removed:false } }
    pub(super) fn recovery_data(&self)->Value {json!({"scope":"own-probe-cleanup-progress-only","unknown":self.unknown,"error_retained":self.error.is_some(),"panic_retained":self.panic.is_some(),"pins_acknowledged":self.unpinned,"stage_removed_ack":self.stage_removed,"resources_closed":self.resources_closed,"broker_lock_unlink_attempted":self.lock_attempted,"broker_lock_unlink_ack":self.lock_removed,"projection_unlink_ack":self.projection_tree.as_ref().is_some_and(|t|t.root_removed),"root_unlink_attempted":self.root_tree.as_ref().is_some_and(|t|t.root_attempted),"root_unlink_ack":self.root_tree.as_ref().is_some_and(|t|t.root_removed),"complete_cleanup_ack":self.done.is_some(),"replay_authorized":false})}
    pub(super) fn acknowledged(&self)->bool { self.done.is_some() }
    pub(super) fn done(&self)->bool { self.done.is_some() && !self.unknown && self.error.is_none() && self.panic.is_none() }
}
fn quiescent(s:&State)->Result<()> {
    ensure!(s.finished && !s.failed && s.error.is_none() && s.panic.is_none(),"probe success/panic boundary unsettled");
    let child=s.child.as_ref().context("original helper absent")?;
    ensure!(child.settled() && child.pidfd.is_some() && child.exited()? && child.status.as_ref().and_then(ExitStatus::code)==Some(143),"actual helper wait/EOF/pidfd proof absent");
    let a=s.control_joined.as_ref().context("actual control/TLS join absent")?; let b=s.handoff_joined.as_ref().context("actual handoff/TLS join absent")?;
    ensure!(a.same_worker(b) && s.broker_joined.is_some(),"actual original worker joins absent/different");
    let shutdown=s.shutdown.as_ref().context("actual shutdown absent")?;
    ensure!(Some(shutdown.request())==s.stop.as_ref() && s.backend.is_some() && s.issuance.as_ref().is_some_and(PreparedCapabilityIssuance::is_retired),"original shutdown/backend/retired issuance absent"); Ok(())
}
fn expected_pins(s:&State)->Result<BTreeMap<String,rudder_engine::protocol::ObjectRef>> {
    let spec=s.spec.as_ref().context("original spec absent")?;
    let observation=s.observation.as_ref().context("original observation ACK absent")?;
    let base=s.source_ref.as_ref().context("original base ACK absent")?;
    let profile=s.profile_ref.as_ref().context("original profile ACK absent")?;
    ensure!(observation.view_id==spec.view_id && observation.owner==spec.owner && observation.request_nonce==s.nonce && observation.pin_id==format!("vfs:observe:{}:{}",spec.view_id,s.nonce),"original observation differs");
    let Some(execution::CheckpointRef::CompleteLocal(checkpoint))=s.shutdown.as_ref().unwrap().record().checkpoint() else { anyhow::bail!("actual complete checkpoint absent") };
    Ok(BTreeMap::from([
        ("kernel-probe-source".into(),base.clone()),("kernel-probe-workspace-profile".into(),profile.clone()),
        (format!("vfs:paged-prepared:{}",base.digest),base.clone()),(format!("vfs:view:{}:base",spec.view_id),base.clone()),
        (observation.pin_id.clone(),observation.directory.clone()),
        (format!("vfs:view:{}:topology:{}",spec.view_id,checkpoint.hardlinks.digest),checkpoint.hardlinks.clone()),
        (format!("vfs:view:{}:checkpoint:{}",spec.view_id,checkpoint.directory.digest),checkpoint.directory.clone()),
    ]))
}
fn cleanup(s:&mut State) -> Result<()> {
    let original=s.retirement.as_ref().unwrap().budget.clone(); let cancel=s.retirement_cancel.as_ref().context("original cleanup cancellation absent")?.clone();
    s.stage="verify-original-own-fixture-quiescence";time(&original,&cancel)?; quiescent(s)?; s.anchors.check_roots(s)?;
    // Current physical custody is required, but no old probe budget, WS status,
    // startup-admission flag or arbitrary project retirement is consulted.
    s.host.as_ref().context("original host absent")?.current()?;
    let pins=expected_pins(s)?; let stage=s.source_stage.as_ref().context("original stage ACK absent")?;
    let retained=s.store.as_ref().unwrap().retention()?;
    ensure!(retained.pins==pins && retained.staging.len()==1 && retained.staging.get(&stage.id)==Some(stage),"fixture has foreign/unknown retention; refuse cleanup");
    s.retirement.as_mut().unwrap().pins=pins.into_iter().collect();
    s.retirement.as_mut().unwrap().lock=Some(s.anchors.broker_lock.as_ref().unwrap().lock_original()?);
    time(&original,&cancel)?;
    let spec=s.spec.as_ref().unwrap();
    s.retirement.as_mut().unwrap().view=Some(s.manager.as_ref().unwrap().open_view(&spec.view_id,&spec.owner)?);
    let view=s.retirement.as_ref().unwrap().view.as_ref().unwrap(); let descriptor=view.descriptor()?;s.anchors.check_view(&spec.view_id)?;
    let Some(execution::CheckpointRef::CompleteLocal(checkpoint))=s.shutdown.as_ref().unwrap().record().checkpoint() else { unreachable!() };
    ensure!(descriptor.view_id==spec.view_id && descriptor.owner==spec.owner && Some(&descriptor.base)==s.source_ref.as_ref() && descriptor.latest_snapshot.as_ref()==Some(checkpoint),"original private view/checkpoint changed");
    time(&original,&cancel)?;
    // Actual owner.lock is held. Original helper/process + namespace-service
    // workers are already joined, so this cannot race a future fixture mount.
    s.stage="retire-original-own-view-and-pins";let destroyed=view.destroy()?;
    s.retirement.as_mut().unwrap().destroyed=Some(destroyed); // ACK before late checks.
    ensure!(s.retirement.as_ref().unwrap().destroyed.as_ref()==Some(checkpoint),"destroy changed original checkpoint"); time(&original,&cancel)?;
    s.retirement.as_ref().unwrap().view.as_ref().unwrap().release_observation(&s.nonce)?;
    // Its exact observation pin is retired by the original VfsView API.
    let store=s.store.as_ref().unwrap();
    for (index,(name,reference)) in s.retirement.as_ref().unwrap().pins.clone().into_iter().enumerate() {
        time(&original,&cancel)?; let state=store.retention()?;
        if name==s.observation.as_ref().unwrap().pin_id { ensure!(!state.pins.contains_key(&name),"observation pin still live"); }
        else { ensure!(state.pins.get(&name)==Some(&reference),"original pin changed before retirement"); store.unpin(&name)?; }
        s.retirement.as_mut().unwrap().unpinned=index+1; time(&original,&cancel)?;
    }
    store.abandon_staging(&stage.id)?; s.retirement.as_mut().unwrap().stage_removed=true; time(&original,&cancel)?;
    let retained=store.retention()?; ensure!(retained.pins.is_empty() && retained.staging.is_empty(),"fixture retention is not empty");
    // Dispose only original, genuinely settled holders, all OFF App. Broker
    // snapshots have narrow filesystem Drop, so they precede tree inventory.
    s.stage="dispose-settled-own-holders-off-app";s.retirement.as_mut().unwrap().view.take();
    s.control.take(); s.receipt.take(); s.activation.take(); s.backend.take(); s.child.take();
    s.control_joined.take(); s.handoff_joined.take(); s.broker.take();
    let old=std::mem::replace(&mut s.broker_owner,BrokerThreadOwner::new()); drop(old); s.broker_joined.take();
    s.journal.take(); s.manager.take(); s.store.take();
    s.retirement.as_mut().unwrap().resources_closed=true; time(&original,&cancel)?;
    s.stage="unlink-original-own-projection-and-namespace";let projection=s.anchors.projection.as_ref().unwrap();
    s.retirement.as_mut().unwrap().projection_tree=Some(Tree::capture(projection,&original,&cancel)?);
    s.retirement.as_mut().unwrap().projection_tree.as_mut().unwrap().remove(projection,&original,&cancel)?;
    let lock=s.anchors.broker_lock.as_ref().unwrap(); lock.check()?;
    time(&original,&cancel)?; let name=CString::new(lock.name.clone())?;
    s.retirement.as_mut().unwrap().lock_attempted=true;
    ensure!(unsafe { libc::unlinkat(lock.parent.as_raw_fd(),name.as_ptr(),0) }==0,"original broker lock unlink uncertain");
    s.retirement.as_mut().unwrap().lock_removed=true; lock.parent.sync_all()?;
    let root=s.anchors.root.as_ref().unwrap();
    let root_identity=(root.identity.dev,root.identity.ino);
    s.retirement.as_mut().unwrap().root_tree=Some(Tree::capture(root,&original,&cancel)?);
    s.retirement.as_mut().unwrap().root_tree.as_mut().unwrap().remove(root,&original,&cancel)?;
    // Close every own actual descriptor OFF App; receipt is staged before any
    // fallible postcheck. No Drop on App ever performs this sequence.
    s.host.as_ref().unwrap().deep(&original,&cancel)?;
    s.retirement.as_mut().unwrap().root_tree.take(); s.retirement.as_mut().unwrap().projection_tree.take(); s.retirement.as_mut().unwrap().lock.take();
    s.root_fd.take(); s.anchors=Anchors::default(); s.host.take(); s.native.take();
    let ack=KernelProbeFixtureRetired { ack:Arc::new(Ack { nonce:s.nonce.clone(),view:s.spec.as_ref().unwrap().view_id.clone(),root:root_identity }) };
    s.retirement.as_mut().unwrap().done=Some(ack); s.stage="own-fixture-retired";
    time(&original,&cancel)
}
/// Caller retains State even on timeout, panic or a late cleanup ACK. A second
/// call never retries a failed effect or substitutes a later budget.
pub(super) fn retire(s:&mut State,original:&FinalizationBudget,cancel:&Arc<AtomicBool>)->Result<KernelProbeFixtureRetired> {
    if let Some(prior)=&s.retirement {
        ensure!(prior.budget.constraint()==original.constraint() && s.retirement_cancel.as_ref().is_some_and(|c|Arc::ptr_eq(c,cancel)),"cleanup budget/cancellation cannot be replaced");
        ensure!(!prior.unknown && prior.error.is_none() && prior.panic.is_none(),"original cleanup requires explicit retained recovery; no replay");
        time(&prior.budget,cancel)?;
        return prior.done.clone().context("original cleanup still owned");
    }
    s.retirement=Some(Retirement::new(original.clone())); s.retirement_cancel=Some(cancel.clone());
    let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||cleanup(s)));
    match result {
        Ok(Ok(()))=>Ok(s.retirement.as_ref().unwrap().done.as_ref().unwrap().clone()),
        Ok(Err(error))=>{ let held=s.retirement.as_mut().unwrap(); held.unknown=true; held.error=Some(error); Err(fault()) },
        Err(panic)=>{ let held=s.retirement.as_mut().unwrap(); held.unknown=true; held.panic=Some(panic); Err(fault()) },
    }
}
#[cfg(test)]
#[path="provider_kernel_retirement_tests.rs"]
mod tests;
