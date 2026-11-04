//! Actual mount mechanics, no Root IPC/registration/task/retirement authority.
//! Run only on a disposable exact-owned Root VM; failure retains unknown mount
//! state until the external owner tears down that VM.
use super::*;
use std::os::unix::fs::DirBuilderExt;
use crate::{directory::{DirectoryEntry,EntryNode,LocalDirectoryStore,PagedDirectory},protocol::OwnerEpoch};
struct Fixture {root:PathBuf,mount:Option<RetainedMetadataMount>}
impl Drop for Fixture {fn drop(&mut self){if self.mount.is_some(){eprintln!("RUNTIME_MOUNT_COMPONENT_UNKNOWN_REQUIRES_OWNED_VM_TEARDOWN");std::process::abort();}}}
#[test]
#[ignore="actual Root/SYS_ADMIN /dev/fuse; mount component only, not runtime grant"]
fn actual_runtime_floor_is_ro_exec_while_source_floor_stays_noexec_and_both_join(){
    assert_eq!(unsafe{libc::geteuid()},0);let root=Path::new("/run").join(format!("rudder-runtime-mount-component-{}",Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let mut owner=Fixture{root,mount:None};
    let store=LocalStore::open(owner.root.join("cas")).unwrap();let stage=store.begin_staging(Duration::from_secs(120)).unwrap();let blob=store.put_blob(b"component bytes\n").unwrap();
    let directory=PagedDirectory::new(&LocalDirectoryStore::new(&store,&stage.id)).build(0o555,[DirectoryEntry::new(b"tool",EntryNode::File{logical_size:blob.size_bytes,object:blob,mode:0o755}).unwrap()]).unwrap();
    store.publish(&stage.id,"component-root",&directory).unwrap();let manager=VfsManager::open(owner.root.join("manager"),store).unwrap();manager.prepare_paged(&directory).unwrap();
    for(mode,noexec)in [(MetadataMountMode::ReadOnlySeed,true),(MetadataMountMode::ReadOnlyRuntimeData,false)]{
        let nonce=Uuid::new_v4().to_string();let target=owner.root.join(&nonce);fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
        let view=manager.fork_paged(&nonce,&directory,OwnerEpoch{owner:"kernel-component-data-only".into(),epoch:1}).unwrap();
        RetainedMetadataMount::capture_retaining(&mut owner.mount,RootMetadataNamespace::capture().unwrap(),view,target.clone(),mode,nonce).unwrap();
        let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();owner.mount.as_mut().unwrap().mount_retaining(&original,&AtomicBool::new(false)).unwrap();
        {let file=open_directory(&target).unwrap();let mut flags:libc::statvfs=unsafe{std::mem::zeroed()};assert_eq!(unsafe{libc::fstatvfs(file.as_raw_fd(),&mut flags)},0);let required=libc::ST_RDONLY|libc::ST_NOSUID|libc::ST_NODEV;assert_eq!(flags.f_flag&required,required);assert_eq!(flags.f_flag&libc::ST_NOEXEC!=0,noexec);}
        assert_eq!(fs::read(target.join("tool")).unwrap(),b"component bytes\n");assert_eq!(OpenOptions::new().write(true).open(target.join("tool")).unwrap_err().raw_os_error(),Some(libc::EROFS));
        owner.mount.as_ref().unwrap().close_admission();let cleanup=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();
        loop{if owner.mount.as_mut().unwrap().unmount_and_checkpoint_retaining(&cleanup,&AtomicBool::new(false)).unwrap(){break}std::thread::sleep(cleanup.phase_limit(Duration::from_millis(2)).unwrap());}
        assert!(owner.mount.as_ref().unwrap().is_unmounted_and_joined());assert!(mount_info(&target).unwrap().is_none());owner.mount=None;
    }
    fs::remove_dir_all(&owner.root).unwrap();
}
