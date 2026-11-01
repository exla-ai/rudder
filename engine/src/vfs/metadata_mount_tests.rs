//! Component tests only. Root ignored cases require actual /dev/fuse and
//! SYS_ADMIN; they never fabricate Native/PG/accepted-source/Root IPC grants.
use super::*;
use std::os::unix::fs::DirBuilderExt;
use crate::{directory::{DirectoryEntry,EntryNode,LocalDirectoryStore,PagedDirectory},protocol::OwnerEpoch};
use std::sync::{Condvar,OnceLock};
static HOOKS:OnceLock<Mutex<HashMap<String,Arc<Hook>>>>=OnceLock::new();
struct Hook {entered:AtomicBool,paused:AtomicBool,release:AtomicBool,tls_entered:AtomicBool,tls_release:AtomicBool,pair:(Mutex<()>,Condvar)}
impl Hook {fn new(paused:bool)->Arc<Self>{Arc::new(Self{entered:AtomicBool::new(false),paused:AtomicBool::new(paused),release:AtomicBool::new(false),tls_entered:AtomicBool::new(false),tls_release:AtomicBool::new(false),pair:(Mutex::new(()),Condvar::new())})}
    fn wake(&self){self.pair.1.notify_all();}}
struct TlsHold(Arc<Hook>);
impl Drop for TlsHold {fn drop(&mut self){self.0.tls_entered.store(true,Ordering::Release);self.0.wake();let mut guard=self.0.pair.0.lock().unwrap();
    while !self.0.tls_release.load(Ordering::Acquire){guard=self.0.pair.1.wait(guard).unwrap();}}}
thread_local! {static HOLD:std::cell::RefCell<Option<TlsHold>>=const{std::cell::RefCell::new(None)};}
pub(super) fn worker_hook(nonce:&str){let hook=HOOKS.get().and_then(|m|m.lock().ok().and_then(|m|m.get(nonce).cloned()));if let Some(hook)=hook{
    HOLD.with(|slot|*slot.borrow_mut()=Some(TlsHold(hook.clone())));hook.entered.store(true,Ordering::Release);hook.wake();
    let mut guard=hook.pair.0.lock().unwrap();while hook.paused.load(Ordering::Acquire)&&!hook.release.load(Ordering::Acquire){guard=hook.pair.1.wait(guard).unwrap();}}}
fn wait(flag:&AtomicBool){let end=std::time::Instant::now()+Duration::from_secs(3);while !flag.load(Ordering::Acquire){assert!(std::time::Instant::now()<end,"actual component transition timed out");std::thread::sleep(Duration::from_millis(1));}}
fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
struct Fixture {root:PathBuf,mount:Option<RetainedMetadataMount>,extra:Vec<RetainedMetadataMount>,manager:VfsManager,base:ObjectRef,hook:Arc<Hook>}
impl Fixture {
    fn capture(paused:bool,mode:MetadataMountMode)->Self{assert_eq!(unsafe{libc::geteuid()},0,"ignored fixture requires actual Root");
        let root=Path::new("/workspace").join(format!("rudder-v2-metadata-kernel-{}",Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let store=LocalStore::open(root.join("cas")).unwrap();let stage=store.begin_staging(Duration::from_secs(120)).unwrap();let object=store.put_blob(b"original\n").unwrap();
        let directory=PagedDirectory::new(&LocalDirectoryStore::new(&store,&stage.id)).build(0o700,[DirectoryEntry::new(b"file",EntryNode::File{logical_size:object.size_bytes,object,mode:0o600}).unwrap()]).unwrap();
        store.publish(&stage.id,"fixture-root",&directory).unwrap();let manager=VfsManager::open(root.join("manager"),store).unwrap();manager.prepare_paged(&directory).unwrap();
        let nonce=Uuid::new_v4().to_string();let view=manager.fork_paged(&nonce,&directory,OwnerEpoch{owner:"root-component-fixture".into(),epoch:1}).unwrap();
        let target=root.join("mount");fs::DirBuilder::new().mode(0o700).create(&target).unwrap();let mut mount=None;
        RetainedMetadataMount::capture_retaining(&mut mount,RootMetadataNamespace::capture().unwrap(),view,target,mode,nonce.clone()).unwrap();
        let hook=Hook::new(paused);HOOKS.get_or_init(||Mutex::new(HashMap::new())).lock().unwrap().insert(nonce,hook.clone());Self{root,mount,extra:Vec::new(),manager,base:directory,hook}
    }
    fn finish(&mut self,original:&FinalizationBudget){self.hook.release.store(true,Ordering::Release);self.hook.tls_release.store(true,Ordering::Release);self.hook.wake();
        loop{if self.mount.as_mut().unwrap().unmount_and_checkpoint_retaining(original,&AtomicBool::new(false)).unwrap(){break}std::thread::sleep(Duration::from_millis(1));}
        for mount in &mut self.extra{loop{if mount.unmount_and_checkpoint_retaining(original,&AtomicBool::new(false)).unwrap(){break}std::thread::sleep(Duration::from_millis(1));}}
        self.extra.clear();if let Some(nonce)=self.mount.as_ref().map(|m|m.nonce.clone()){if let Some(hooks)=HOOKS.get(){hooks.lock().unwrap().remove(&nonce);}}
        self.mount=None;fs::remove_dir_all(&self.root).unwrap();}
}
impl Drop for Fixture {fn drop(&mut self){
    self.hook.release.store(true,Ordering::Release);self.hook.tls_release.store(true,Ordering::Release);self.hook.wake();
    if self.mount.is_some()||!self.extra.is_empty(){
        // Assertion failure must not pretend original mounts/threads settled.
        // These ignored tests run ONLY on disposable VMs whose external owner
        // destroys the VM after this explicit fail-closed process termination.
        eprintln!("ROOT_METADATA_COMPONENT_RECOVERY_REQUIRES_VM_DESTRUCTION");std::process::abort();
    }
}}
#[test]
fn non_root_cannot_construct_global_metadata_namespace(){if unsafe{libc::geteuid()}!=0{assert!(RootMetadataNamespace::capture().is_err());}}

#[test]
#[ignore="actual Root SYS_ADMIN/dev-fuse current namespace; run only disposable VM"]
fn actual_tls_blocks_checkpoint_after_normal_unmount_and_callback_result(){
    let mut f=Fixture::capture(false,MetadataMountMode::PrivateCow);let original=budget();f.mount.as_mut().unwrap().mount_retaining(&original,&AtomicBool::new(false)).unwrap();
    assert_eq!(fs::read(f.mount.as_ref().unwrap().root().unwrap().join("file")).unwrap(),b"original\n");
    let cleanup=budget();assert!(!f.mount.as_mut().unwrap().unmount_and_checkpoint_retaining(&cleanup,&AtomicBool::new(false)).unwrap());
    wait(&f.hook.tls_entered);assert!(f.mount.as_ref().unwrap().checkpoint().is_err());
    assert!(!f.mount.as_mut().unwrap().unmount_and_checkpoint_retaining(&cleanup,&AtomicBool::new(false)).unwrap());
    f.finish(&cleanup);
}

#[test]
#[ignore="actual Root SYS_ADMIN/dev-fuse current namespace; run only disposable VM"]
fn cancellation_during_actual_fuse_handshake_retains_worker_device_and_mount(){
    let mut f=Fixture::capture(true,MetadataMountMode::ReadOnlySeed);let original=budget();let cancelled=Arc::new(AtomicBool::new(false));
    let cancel=cancelled.clone();let hook=f.hook.clone();let cancellation=std::thread::spawn(move||{wait(&hook.entered);cancel.store(true,Ordering::Release);});
    assert!(f.mount.as_mut().unwrap().mount_retaining(&original,&cancelled).is_err());cancellation.join().unwrap();
    let owner=f.mount.as_ref().unwrap();assert!(!owner.ready);assert!(owner.device.is_some()&&owner.worker.is_some()&&owner.mount_id.is_some());assert!(owner.checkpoint().is_err());
    // A NEW explicit cleanup phase does not renew the failed preparation.
    let cleanup=budget();f.finish(&cleanup);
}


#[test]
#[ignore="actual Root SYS_ADMIN/dev-fuse; UID1500 child and mounts only on disposable VM"]
fn actual_uid1500_reads_ro_and_writes_two_shared_floor_cow_mounts_in_isolation(){
    use std::os::unix::process::CommandExt;
    let mut f=Fixture::capture(false,MetadataMountMode::ReadOnlySeed);fs::set_permissions(&f.root,fs::Permissions::from_mode(0o711)).unwrap();
    let original=budget();f.mount.as_mut().unwrap().mount_retaining(&original,&AtomicBool::new(false)).unwrap();
    let ro=f.mount.as_ref().unwrap().root().unwrap().to_owned();
    assert_eq!(fs::write(ro.join("file"),b"root forbidden").unwrap_err().raw_os_error(),Some(libc::EROFS));
    let namespace=f.mount.as_ref().unwrap().namespace.clone();let mut roots=Vec::new();
    for name in ["private-a","private-b"]{let path=f.root.join(name);fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let nonce=Uuid::new_v4().to_string();let view=f.manager.fork_paged(&nonce,&f.base,OwnerEpoch{owner:"root-component-fixture".into(),epoch:1}).unwrap();
        let mut owner=None;RetainedMetadataMount::capture_retaining(&mut owner,namespace.clone(),view,path.clone(),MetadataMountMode::PrivateCow,nonce).unwrap();
        f.extra.push(owner.unwrap());f.extra.last_mut().unwrap().mount_retaining(&original,&AtomicBool::new(false)).unwrap();roots.push(path);}
    let mut child=std::process::Command::new(std::env::current_exe().unwrap());
    child.args(["--ignored","--exact","vfs::linux::metadata::tests::actual_uid1500_component_child","--nocapture"])
        .env_clear().env("RUDDER_V2_COMPONENT_RO",&ro).env("RUDDER_V2_COMPONENT_A",&roots[0]).env("RUDDER_V2_COMPONENT_B",&roots[1]);
    unsafe{child.pre_exec(||{
        if libc::setgroups(0,std::ptr::null())!=0||libc::setresgid(1500,1500,1500)!=0||libc::setresuid(1500,1500,1500)!=0||libc::prctl(libc::PR_SET_NO_NEW_PRIVS,1,0,0,0)!=0{return Err(std::io::Error::last_os_error())}Ok(())
    });}
    // This is the actual Child wait/reap, not a callback/result-channel proof.
    let mut child=child.spawn().unwrap();let end=std::time::Instant::now()+Duration::from_secs(10);let status=loop{
        if let Some(status)=child.try_wait().unwrap(){break status}if std::time::Instant::now()>=end{child.kill().unwrap();let _=child.wait();panic!("UID1500 component child deadline")};std::thread::sleep(Duration::from_millis(1));};
    assert!(status.success());assert_eq!(fs::read(ro.join("file")).unwrap(),b"original\n");
    assert_eq!(fs::read(roots[0].join("file")).unwrap(),b"private A\n");assert_eq!(fs::read(roots[1].join("file")).unwrap(),b"private B\n");
    f.finish(&budget());
}

#[test]
#[ignore="child only: spawned with actual UID/GID1500 and NNP by Root component fixture"]
fn actual_uid1500_component_child(){
    let ro=std::env::var_os("RUDDER_V2_COMPONENT_RO").expect("only actual Root component parent may invoke child fixture");
    assert_eq!(unsafe{libc::getuid()},1500);assert_eq!(unsafe{libc::geteuid()},1500);assert_eq!(unsafe{libc::getgid()},1500);
    assert_eq!(unsafe{libc::prctl(libc::PR_GET_NO_NEW_PRIVS,0,0,0,0)},1);
    let status=fs::read_to_string("/proc/self/status").unwrap();assert!(status.lines().any(|l|l=="CapEff:\t0000000000000000"));
    let ro=PathBuf::from(ro);let a=PathBuf::from(std::env::var_os("RUDDER_V2_COMPONENT_A").unwrap());let b=PathBuf::from(std::env::var_os("RUDDER_V2_COMPONENT_B").unwrap());
    assert_eq!(fs::read(ro.join("file")).unwrap(),b"original\n");assert_eq!(fs::write(ro.join("file"),b"forbidden").unwrap_err().raw_os_error(),Some(libc::EROFS));
    fs::write(a.join("file"),b"private A\n").unwrap();assert_eq!(fs::read(b.join("file")).unwrap(),b"original\n");fs::write(b.join("file"),b"private B\n").unwrap();
    assert_eq!(fs::read(a.join("file")).unwrap(),b"private A\n");
}
