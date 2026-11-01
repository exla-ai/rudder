//! Explicit Root-owned seed/metadata mounts in the CURRENT mount namespace.
//! No pathname, saved JSON or clock timeout issues a join/checkpoint receipt.
//! Keep this mutable carrier outside catch_unwind. Drop does no unmount/join.
use super::*;
use crate::execution::{finalization_budget::FinalizationBudget,
    handoff::{OwnedWorkerJoin,WorkerJoined}};
use std::{os::fd::{AsRawFd,FromRawFd,OwnedFd},sync::atomic::AtomicU8};

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum MetadataMountMode { ReadOnlySeed, PrivateCow,
    /// Mechanical immutable EXEC-capable floor only. This is DATA, not a
    /// runtime grant: execution independently requires the original Root
    /// service/launcher/registered-engine capsule and held component FDs.
    ReadOnlyRuntimeData }
impl MetadataMountMode {
    fn readonly(self)->bool {self!=Self::PrivateCow}
    fn noexec(self)->bool {self!=Self::ReadOnlyRuntimeData}
}
/// Actual Root/process/MNT/boot custody, never deserialized or reconstructed.
pub struct RootMetadataNamespace {pid:u32,pidfd:OwnedFd,namespace:File,identity:(u64,u64),boot:File,boot_bytes:Vec<u8>}
impl RootMetadataNamespace {
    pub fn capture()->Result<Arc<Self>> {
        ensure!(unsafe{libc::getuid()}==0&&unsafe{libc::geteuid()}==0,"metadata mount requires original Root");
        let pid=std::process::id();let raw=unsafe{libc::syscall(libc::SYS_pidfd_open,pid,0)} as i32;
        ensure!(raw>=0,"metadata Root pidfd unavailable");let pidfd=unsafe{OwnedFd::from_raw_fd(raw)};
        let namespace=File::open("/proc/thread-self/ns/mnt")?;let m=namespace.metadata()?;
        ensure!(unsafe{libc::ioctl(namespace.as_raw_fd(),libc::NS_GET_NSTYPE as libc::c_ulong)}==libc::CLONE_NEWNS,"metadata namespace is not MNT");
        let boot=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open("/proc/sys/kernel/random/boot_id")?;
        let value=Arc::new(Self{pid,pidfd,namespace,identity:(m.dev(),m.ino()),boot_bytes:boot_read(&boot)?,boot});value.check_current()?;Ok(value)
    }
    pub fn check_current(&self)->Result<()> {
        ensure!(std::process::id()==self.pid&&unsafe{libc::getuid()}==0&&unsafe{libc::geteuid()}==0,"metadata Root process changed");
        let mut p=libc::pollfd{fd:self.pidfd.as_raw_fd(),events:libc::POLLIN,revents:0};
        ensure!(unsafe{libc::poll(&mut p,1,0)}==0,"metadata original Root exited");
        let named=File::open("/proc/thread-self/ns/mnt")?.metadata()?;let held=self.namespace.metadata()?;
        ensure!((named.dev(),named.ino())==self.identity&&(held.dev(),held.ino())==self.identity&&boot_read(&self.boot)?==self.boot_bytes,"metadata namespace/boot changed");Ok(())
    }
}
fn boot_read(file:&File)->Result<Vec<u8>>{let mut b=[0;65];let n=file.read_at(&mut b,0)?;
    ensure!(n==37&&b[36]==b'\n'&&Uuid::parse_str(std::str::from_utf8(&b[..36])?)?.to_string().as_bytes()==&b[..36],"metadata boot ID invalid");Ok(b[..n].to_vec())}
fn tick(original:&FinalizationBudget,cancel:&AtomicBool)->Result<()>{if cancel.load(Ordering::Acquire){original.cancel();bail!("metadata original request cancelled");}original.remaining()?;Ok(())}
fn open_directory(path:&Path)->Result<File>{Ok(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?)}
fn root_parent(path:&Path)->Result<()> {
    ensure!(path.is_absolute()&&path.canonicalize()?==path,"metadata mount path redirects");
    for p in path.ancestors(){let m=fs::symlink_metadata(p)?;ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"metadata mount parent is not protected Root custody");}Ok(())
}
fn mount_info(path:&Path)->Result<Option<(u64,String,String)>> {
    let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open("/proc/thread-self/mountinfo")?;
    let mut bytes=Vec::new();file.take((1<<20)+1).read_to_end(&mut bytes)?;ensure!(bytes.len()<=1<<20,"metadata mountinfo exceeds bound");
    let path=path.to_str().context("metadata mount path not UTF8")?;ensure!(!path.bytes().any(|b|b.is_ascii_whitespace()||b==b'\\'),"metadata mount path requires unsupported escaping");
    let mut found=None;for line in std::str::from_utf8(&bytes)?.lines(){let(l,r)=line.split_once(" - ").context("metadata mountinfo incomplete")?;
        let l=l.split(' ').collect::<Vec<_>>();let r=r.split(' ').collect::<Vec<_>>();ensure!(l.len()>=6&&r.len()>=3,"metadata mountinfo malformed");
        if l[4]==path{ensure!(found.is_none(),"metadata mount identity ambiguous");found=Some((l[0].parse()?,r[0].into(),l[5].into()));}}
    Ok(found)
}
struct WorkerResult {result:Mutex<Option<std::io::Result<()>>>,initialized:AtomicBool}
/// Proof keeps the pinned private checkpoint and SAME original TLS join.
/// Its roots are DATA; it exposes no live view/input/resume API.
pub struct MetadataCowCheckpoint {view_id:String,store:LocalStore,snapshot:VfsCheckpoint,_joined:WorkerJoined,mount_nonce:String}
impl MetadataCowCheckpoint {
    pub fn snapshot(&self)->&VfsCheckpoint{&self.snapshot}
    pub fn mount_nonce(&self)->&str{&self.mount_nonce}
    pub fn view_id(&self)->&str{&self.view_id}
    pub fn check_retained(&self)->Result<()>{ensure!(self.store.retention()?.pins.values().any(|r|r==&self.snapshot.directory),"private CoW checkpoint pin lost");Ok(())}
}
/// Bounded carrier: one original FUSE worker plus one retained join observer.
/// A returned error never drops the worker, original device or view.
pub struct RetainedMetadataMount {
    namespace:Arc<RootMetadataNamespace>,view:Option<VfsView>,path:PathBuf,parent:Option<File>,parent_id:Option<(u64,u64)>,
    mode:MetadataMountMode,nonce:String,source:String,device:Option<OwnedFd>,underlying:Option<File>,root:Option<File>,
    mount_attempted:bool,mount_id:Option<u64>,unmount_attempted:bool,unmounted:bool,
    worker:Option<OwnedWorkerJoin>,joined:Option<WorkerJoined>,result:Arc<WorkerResult>,
    admission:AtomicU8,cleanup_original:Option<FinalizationBudget>,checkpoint:Option<MetadataCowCheckpoint>,checkpoint_attempted:bool,ready:bool,
}
impl RetainedMetadataMount {
    /// Path must already be a fresh empty Root-owned directory under the
    /// actual held anchor. Native receives it only after Root's original ACK.
    pub fn capture_retaining(out:&mut Option<Self>,namespace:Arc<RootMetadataNamespace>,view:VfsView,
        path:PathBuf,mode:MetadataMountMode,nonce:String)->Result<()> {
        ensure!(out.is_none(),"metadata mount carrier occupied");
        // The actual already-created VFS view is owned BEFORE any fallible
        // namespace/nonce/path check. Failed capture keeps its DB/lock owner.
        *out=Some(Self {namespace,view:Some(view),path,parent:None,parent_id:None,mode,source:format!("rudder-seed-{nonce}"),nonce,
            device:None,underlying:None,root:None,mount_attempted:false,mount_id:None,unmount_attempted:false,unmounted:false,
            worker:None,joined:None,result:Arc::new(WorkerResult{result:Mutex::new(None),initialized:AtomicBool::new(false)}),
            admission:AtomicU8::new(0),cleanup_original:None,checkpoint:None,checkpoint_attempted:false,ready:false});
        out.as_mut().unwrap().capture_target()
    }
    fn capture_target(&mut self)->Result<()> {
        ensure!(Uuid::parse_str(&self.nonce)?.to_string()==self.nonce,"metadata mount nonce invalid");self.namespace.check_current()?;
        let parent_path=self.path.parent().context("metadata mount parent absent")?;root_parent(parent_path)?;
        self.parent=Some(open_directory(parent_path)?);let m=self.parent.as_ref().unwrap().metadata()?;self.parent_id=Some((m.dev(),m.ino()));
        self.underlying=Some(open_directory(&self.path)?);let u=self.underlying.as_ref().unwrap().metadata()?;
        ensure!(u.uid()==0&&u.gid()==0&&u.mode()&0o7777==0o700&&fs::read_dir(&self.path)?.next().is_none()&&mount_info(&self.path)?.is_none(),"metadata mount target not fresh Root directory");Ok(())
    }
    fn parent_current(&self)->Result<()> {self.namespace.check_current()?;
        let named=fs::symlink_metadata(self.path.parent().context("metadata original parent absent")?)?;
        let held=self.parent.as_ref().context("metadata original parent capture incomplete")?.metadata()?;
        ensure!(Some((named.dev(),named.ino()))==self.parent_id&&Some((held.dev(),held.ino()))==self.parent_id&&named.uid()==0&&named.gid()==0&&named.mode()&0o022==0,"metadata mount parent replaced");Ok(())}
    pub fn mount_retaining(&mut self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        tick(original,cancel)?;self.parent_current()?;ensure!(!self.mount_attempted&&self.worker.is_none(),"metadata mount already attempted");
        let device=OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open("/dev/fuse")?;
        ensure!(device.metadata()?.file_type().is_char_device(),"metadata FUSE device absent");self.device=Some(device.into());
        let target=CString::new(self.path.as_os_str().as_bytes())?;
        let options=CString::new(format!("fd={},rootmode=40700,user_id=1500,group_id=1500,default_permissions,allow_other",self.device.as_ref().unwrap().as_raw_fd()))?;
        ensure!(!self.view.as_ref().context("metadata view absent")?.0.mounted.swap(true,Ordering::AcqRel),"metadata view already mounted");
        self.mount_attempted=true;let ro=if self.mode.readonly(){libc::MS_RDONLY}else{0};
        let noexec=if self.mode.noexec(){libc::MS_NOEXEC}else{0};
        if unsafe{libc::mount(CString::new(self.source.clone())?.as_ptr(),target.as_ptr(),c"fuse.rudder-seed".as_ptr(),
            libc::MS_NOSUID|libc::MS_NODEV|noexec|ro,options.as_ptr().cast())}!=0{
            // A synchronous failed mount syscall has no mount effect; retain
            // original device/view and allow only explicit cleanup.
            return Err(std::io::Error::last_os_error().into());}
        self.mount_id=Some(mount_info(&self.path)?.context("metadata original mount absent")?.0);
        let input=self.device.as_ref().unwrap().try_clone()?;let nonce=self.nonce.clone();let view=self.view.as_ref().context("metadata view absent")?.clone();let result=self.result.clone();
        // Session::from_fd performs handshake. It runs ONLY in the retained
        // original worker; the original device stays in this outer carrier.
        self.worker=Some(OwnedWorkerJoin::new(std::thread::Builder::new().name("rudder-metadata-fuse".into()).spawn(move||{
            #[cfg(test)] tests::worker_hook(&nonce);
            let mut config=Config::default();config.n_threads=Some(1);config.acl=SessionACL::All;
            let answer=Session::from_fd(Adapter{view,uid:1500,gid:1500},input,SessionACL::All,config).and_then(|session|{
                result.initialized.store(true,Ordering::Release);session.run()});
            if let Ok(mut slot)=result.result.lock(){*slot=Some(answer);}
        })?));
        while !self.result.initialized.load(Ordering::Acquire){tick(original,cancel)?;
            if self.result.result.lock().map_err(|_|error(libc::EIO))?.is_some(){bail!("metadata original FUSE handshake failed; owner retained");}
            std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
        self.root=Some(open_directory(&self.path)?);self.check_current()?;tick(original,cancel)?;self.ready=true;Ok(())
    }
    pub fn check_current(&self)->Result<()> {
        self.parent_current()?;ensure!(!self.unmounted&&self.admission.load(Ordering::Acquire)<2,"metadata mount retired");
        let(id,kind,options)=mount_info(&self.path)?.context("metadata actual mount absent")?;
        ensure!(self.mount_id==Some(id)&&kind=="fuse.rudder-seed"&&options.split(',').any(|s|s=="nosuid")
            &&options.split(',').any(|s|s=="nodev")&&options.split(',').any(|s|s=="noexec")==self.mode.noexec()
            &&options.split(',').any(|s|s==if self.mode.readonly(){"ro"}else{"rw"}),"metadata kernel mount differs");
        ensure!(self.result.result.lock().map_err(|_|error(libc::EIO))?.is_none(),"metadata worker exited before original retirement");Ok(())
    }
    pub fn root(&self)->Result<&Path>{ensure!(self.ready,"metadata mount original ready ACK absent");self.check_current()?;Ok(&self.path)}
    /// Mechanical held-FD projection. No caller pathname, Root UID check or
    /// this DATA pair itself authorizes helper execution. The native issuer
    /// must retain its separately authenticated complete bootstrap chain.
    pub fn duplicate_runtime_root_retaining(&self)->Result<(OwnedFd,u64)> {
        ensure!(self.mode==MetadataMountMode::ReadOnlyRuntimeData&&self.ready,
            "runtime floor original mount ACK absent");self.check_current()?;
        let fd:OwnedFd=self.root.as_ref().context("runtime original root FD absent")?.try_clone()?.into();
        self.check_current()?;Ok((fd,self.mount_id.context("runtime mount identity absent")?))
    }
    pub fn view(&self)->Result<&VfsView>{self.view.as_ref().context("live metadata view already archived")}
    pub fn close_admission(&self){self.admission.fetch_max(1,Ordering::AcqRel);}
    /// Caller independently owns the genuine actor/helper/alias drain proof.
    /// Normal umount is additionally mandatory and may refuse EBUSY forever.
    /// No is_finished/callback/channel receipt is accepted as actual TLS join.
    pub fn unmount_and_checkpoint_retaining(&mut self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<bool> {
        self.close_admission();if let Some(prior)=&self.cleanup_original{ensure!(prior.constraint()==original.constraint(),"metadata cleanup clock replaced");}
        else{tick(original,cancel)?;self.cleanup_original=Some(original.clone());}
        tick(original,cancel)?;self.parent_current()?;
        if self.admission.load(Ordering::Acquire)==2{return Ok(true)}
        if !self.unmounted {
            if let Some((id,kind,_))=mount_info(&self.path)?{ensure!(self.mount_id.is_none_or(|expected|id==expected)&&kind=="fuse.rudder-seed","metadata cleanup refuses replaced mount");
                drop(self.root.take());self.unmount_attempted=true;let target=CString::new(self.path.as_os_str().as_bytes())?;
                if unsafe{libc::umount2(target.as_ptr(),0)}!=0{self.unmount_attempted=false;return Err(std::io::Error::last_os_error().into());}}
            else{ensure!(!self.mount_attempted||self.mount_id.is_none()||self.unmount_attempted,"metadata mount disappeared before original normal unmount");}
            self.unmounted=true;self.device=None;
        }
        ensure!(mount_info(&self.path)?.is_none(),"metadata normal unmount unconfirmed");
        if let Some(worker)=&self.worker {worker.start_join_observer()?;if self.joined.is_none(){self.joined=worker.try_joined()?;}if self.joined.is_none(){return Ok(false)}
            let result=self.result.result.lock().map_err(|_|error(libc::EIO))?;
            ensure!(result.as_ref().is_some_and(|r|r.is_ok())||(!self.ready&&self.mode.readonly()&&result.is_some()),"metadata worker result uncertain; view retained");}
        // Only a never-spawned worker can lack join custody. Such a failed
        // preparation cleanup never issues a private checkpoint.
        self.view.as_ref().context("metadata view absent")?.0.mounted.store(false,Ordering::Release);self.view.as_ref().unwrap().0.handles.lock().map_err(|_|error(libc::EIO))?.clear();
        tick(original,cancel)?;
        if self.mode==MetadataMountMode::PrivateCow&&self.checkpoint.is_none(){
            ensure!(!self.checkpoint_attempted,"original metadata checkpoint outcome unknown; actual view retained, no replay");self.checkpoint_attempted=true;
            ensure!(self.joined.is_some(),"private CoW checkpoint requires actual original worker join");
            let view=self.view.as_ref().context("metadata view absent")?;
            let snapshot=view.seal()?;
            let view_id=view.id().to_owned();let store=view.manager().store().clone();
            let joined=self.joined.take().unwrap();
            // The actual pinned output is stored BEFORE fallible late checks.
            self.checkpoint=Some(MetadataCowCheckpoint{view_id,store,snapshot,_joined:joined,mount_nonce:self.nonce.clone()});}
        self.view=None; // OFF App, AFTER actual worker/TLS join and durable checkpoint.
        self.admission.store(2,Ordering::Release);tick(original,cancel)?;Ok(true)
    }
    /// Move the genuine pin+join receipt before dropping final mount/view FDs.
    /// This permits bounded archived history without retaining one SQLite DB
    /// and mount-parent FD per completed task. No path re-adoption occurs.
    pub fn take_checkpoint_retaining(&mut self,out:&mut Option<MetadataCowCheckpoint>)->Result<()> {
        ensure!(out.is_none()&&self.unmounted&&self.view.is_none()&&self.admission.load(Ordering::Acquire)==2,"metadata archive cleanup incomplete");
        self.checkpoint.as_ref().context("original metadata checkpoint absent")?.check_retained()?;
        *out=self.checkpoint.take();Ok(())
    }
    pub fn checkpoint(&self)->Result<&MetadataCowCheckpoint>{self.checkpoint.as_ref().context("original metadata checkpoint ACK absent")}
    pub fn is_unmounted_and_joined(&self)->bool{self.unmounted&&(self.worker.is_none()||self.joined.is_some()||self.checkpoint.is_some())}
}

#[cfg(test)]
#[path="metadata_mount_tests.rs"]
mod tests;

#[cfg(test)]
#[path="runtime_mount_tests.rs"]
mod runtime_mount_tests;
