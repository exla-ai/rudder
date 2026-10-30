//! The mounted suite compiles a genuinely fresh single-thread helper. Normal CI
//! only builds it. Run the emitted binary as root inside an owned disposable VM;
//! never change /dev/fuse permissions on a developer/active worker host.
#![cfg(target_os = "linux")]
const DRIVER: &str = r###"
use rudder_engine::{protocol::{ObjectRef,OwnerEpoch},store::{LocalStore,DirectoryManifest,DirectoryEntry,EntryNode},vfs::{VfsManager,VfsView,linux::{mount,PrivateMountNamespace}}};
use std::{fs,io::{Read,Write,Seek,SeekFrom},path::{Path,PathBuf},os::{fd::AsRawFd,unix::{ffi::OsStringExt,fs::{MetadataExt,PermissionsExt,symlink}}},time::{Instant,Duration}};
use uuid::Uuid;
fn owner()->OwnerEpoch{OwnerEpoch{owner:"qualified-fuse".into(),epoch:1}}
fn dir(s:&LocalStore,e:Vec<DirectoryEntry>)->ObjectRef{s.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:e}).unwrap()}
fn f(blob:&ObjectRef,name:&[u8])->DirectoryEntry{DirectoryEntry::new(name,EntryNode::File{object:blob.clone(),mode:0o644}).unwrap()}
fn main(){unsafe{let limit=libc::rlimit{rlim_cur:0,rlim_max:0};libc::setrlimit(libc::RLIMIT_CORE,&limit);};let a=std::env::args().collect::<Vec<_>>();let root=PathBuf::from(&a[2]);if a[1]=="hold"{unsafe{libc::setsid();}let mut f=fs::OpenOptions::new().read(true).write(true).open(&a[2]).unwrap();f.write_all(b"descendant").unwrap();f.sync_all().unwrap();fs::write(&a[3],b"ready").unwrap();std::thread::sleep(Duration::from_secs(30));return;}
if a[1]=="as-user"||a[1]=="missing-device"{unsafe{assert_eq!(libc::setgroups(0,std::ptr::null()),0);assert_eq!(libc::setgid(1500),0);assert_eq!(libc::setuid(1500),0);}use std::os::unix::process::CommandExt;let mode=if a[1]=="missing-device"{"missing-device-user"}else{"qualify-user"};panic!("fresh UID1500 exec failed: {}",std::process::Command::new(std::env::current_exe().unwrap()).args([mode,&a[2]]).exec());}if unsafe{libc::geteuid()}!=0{assert!(fs::read("/tmp/vfs-root-only/secret").is_err());assert!(fs::read_dir("/root").is_err());if let Ok(fd)=fs::File::open("/proc/1/ns/mnt"){assert_eq!(unsafe{libc::setns(fd.as_raw_fd(),libc::CLONE_NEWNS)},-1);}}
let namespace=if unsafe{libc::geteuid()}==0{PrivateMountNamespace::enter()}else{PrivateMountNamespace::enter_for_current_user()}.unwrap();let s=LocalStore::open(root.join("cas")).unwrap();let m=VfsManager::open(root.join("manager"),s.clone()).unwrap();
if a[1]=="crash"{let v=m.open_view(&a[3],&owner()).unwrap();let p=m.root().join("mounts").join(v.id());let _mounted=mount(v,&p,&namespace).unwrap();let mut file=fs::OpenOptions::new().read(true).write(true).open(p.join("file")).unwrap();file.seek(SeekFrom::Start(0)).unwrap();file.write_all(b"FUSE-fsynced").unwrap();file.sync_all().unwrap();drop(file);fs::rename(p.join("file"),p.join("acknowledged")).unwrap();fs::File::open(&p).unwrap().sync_all().unwrap();fs::write(root.join("crash-ack"),b"namespace and fsync acknowledged").unwrap();std::process::abort();}
if a[1]=="missing-device-user"{let blob=s.put_blob(b"body").unwrap();let base=dir(&s,vec![f(&blob,b"file")]);m.prepare(&base).unwrap();let view=m.fork(&Uuid::new_v4().to_string(),&base,owner()).unwrap();let path=m.root().join("mounts").join(view.id());assert!(mount(view,&path,&namespace).is_err());println!("{}",serde_json::json!({"missing_device_fails_closed":true,"uid":unsafe{libc::geteuid()}}));return;}
let mut results=vec![];let blob=s.put_blob(b"inherited payload").unwrap();
for count in [1,100,10_000,100_000]{eprintln!("phase=prepare count={count}");let lower=dir(&s,(0..count).map(|i|f(&blob,format!("f{i:06}").as_bytes())).collect());let empty=dir(&s,vec![]);let base=dir(&s,vec![DirectoryEntry::new(b"tree",EntryNode::Directory{object:lower}).unwrap(),DirectoryEntry::new(b"dest",EntryNode::Directory{object:empty}).unwrap(),f(&blob,b"a"),f(&blob,b"equal"),f(&blob,b"file")]);let start=Instant::now();m.prepare(&base).unwrap();let prepare=start.elapsed();let mut fork_samples=Vec::new();let mut rename_samples=Vec::new();for _ in 0..5{let start=Instant::now();let trial=m.fork(&Uuid::new_v4().to_string(),&base,owner()).unwrap();fork_samples.push(start.elapsed().as_nanos());assert_eq!(trial.descriptor().unwrap().inode_count,1);let tp=m.root().join("mounts").join(trial.id());let mut tm=mount(trial.clone(),&tp,&namespace).unwrap();let start=Instant::now();fs::rename(tp.join("tree"),tp.join("dest/moved")).unwrap();rename_samples.push(start.elapsed().as_nanos());assert_eq!(trial.descriptor().unwrap().inode_count,3);assert_eq!(fs::metadata(&tp).unwrap().nlink(),3);assert_eq!(fs::metadata(tp.join("dest")).unwrap().nlink(),3);tm.unmount().unwrap();}let start=Instant::now();let v=m.fork(&Uuid::new_v4().to_string(),&base,owner()).unwrap();let fork=start.elapsed();assert_eq!(v.descriptor().unwrap().inode_count,1);let p=m.root().join("mounts").join(v.id());let mut mounted=mount(v.clone(),&p,&namespace).unwrap();eprintln!("phase=mounted count={count}");
let start=Instant::now();fs::rename(p.join("tree"),p.join("dest/moved")).unwrap();let rename=start.elapsed();assert_eq!(fs::metadata(&p).unwrap().nlink(),3);assert_eq!(fs::metadata(p.join("dest")).unwrap().nlink(),3);assert_eq!(v.descriptor().unwrap().inode_count,3);assert!(!p.join("tree").exists());assert_eq!(fs::read(p.join("dest/moved").join(format!("f{:06}",count-1))).unwrap(),b"inherited payload");
if count>1{mounted.unmount().unwrap();results.push(serde_json::json!({"entries":count,"prepare_ns":prepare.as_nanos(),"fork_ns":fork.as_nanos(),"rename_syscall_ns":rename.as_nanos(),"inodes_after_rename":3,"fork_samples_ns":fork_samples,"rename_samples_ns":rename_samples}));continue;}eprintln!("phase=identity");assert_ne!(fs::metadata(p.join("a")).unwrap().ino(),fs::metadata(p.join("equal")).unwrap().ino());let mut held=fs::OpenOptions::new().read(true).write(true).open(p.join("a")).unwrap();fs::hard_link(p.join("a"),p.join("alias")).unwrap();assert_eq!(fs::metadata(p.join("a")).unwrap().ino(),fs::metadata(p.join("alias")).unwrap().ino());assert_eq!(fs::metadata(p.join("alias")).unwrap().nlink(),2);held.write_all(b"private").unwrap();held.sync_all().unwrap();assert!(fs::read(p.join("alias")).unwrap().starts_with(b"private"));assert_eq!(fs::read(p.join("equal")).unwrap(),b"inherited payload");
fs::remove_file(p.join("a")).unwrap();fs::remove_file(p.join("alias")).unwrap();held.seek(SeekFrom::Start(0)).unwrap();let mut b=vec![];held.read_to_end(&mut b).unwrap();assert!(b.starts_with(b"private"));assert!(mounted.unmount().is_err());assert!(v.checkpoint_snapshot_unmounted().is_err());drop(held);
eprintln!("phase=descendant");let ready=root.join("writer-ready");let mut child=std::process::Command::new(std::env::current_exe().unwrap()).args(["hold",p.join("file").to_str().unwrap(),ready.to_str().unwrap()]).spawn().unwrap();let deadline=Instant::now()+Duration::from_secs(10);while !ready.exists(){assert!(Instant::now()<deadline,"descendant did not start");std::thread::sleep(Duration::from_millis(10));}assert!(mounted.unmount().is_err());child.kill().unwrap();child.wait().unwrap();
eprintln!("phase=mmap");let mapped=fs::OpenOptions::new().read(true).write(true).open(p.join("file")).unwrap();unsafe{let ptr=libc::mmap(std::ptr::null_mut(),17,libc::PROT_READ|libc::PROT_WRITE,libc::MAP_SHARED,mapped.as_raw_fd(),0);assert_ne!(ptr,libc::MAP_FAILED);std::ptr::copy_nonoverlapping(b"MAPPED".as_ptr(),ptr.cast::<u8>(),6);assert_eq!(libc::msync(ptr,17,libc::MS_SYNC),0);assert_eq!(libc::munmap(ptr,17),0);}mapped.sync_all().unwrap();drop(mapped);assert!(fs::read(p.join("file")).unwrap().starts_with(b"MAPPED"));
fs::write(p.join("save.tmp"),b"atomic save").unwrap();fs::rename(p.join("save.tmp"),p.join("equal")).unwrap();assert_eq!(fs::read(p.join("equal")).unwrap(),b"atomic save");fs::set_permissions(p.join("equal"),fs::Permissions::from_mode(0o751)).unwrap();assert_eq!(fs::metadata(p.join("equal")).unwrap().mode()&0o7777,0o751);
let raw=std::ffi::OsString::from_vec(b"raw-\xff".to_vec());fs::write(p.join(&raw),b"raw bytes").unwrap();symlink(&raw,p.join("link")).unwrap();assert_eq!(fs::read_link(p.join("link")).unwrap(),PathBuf::from(&raw));assert_eq!(fs::read(p.join("link")).unwrap(),b"raw bytes");
fs::hard_link(p.join("file"),p.join("file-alias")).unwrap();fs::File::open(&p).unwrap().sync_all().unwrap();eprintln!("phase=unmount");let view=mounted.unmount().unwrap();eprintln!("phase=checkpoint");let cp=view.checkpoint_snapshot_unmounted().unwrap();m.prepare_snapshot(&cp).unwrap();eprintln!("phase=restore");let restored=m.fork_snapshot(&Uuid::new_v4().to_string(),&cp,owner()).unwrap();let rp=m.root().join("mounts").join(restored.id());let mut remount=mount(restored.clone(),&rp,&namespace).unwrap();assert!(fs::read(rp.join("file")).unwrap().starts_with(b"MAPPED"));assert_eq!(fs::metadata(rp.join("file")).unwrap().ino(),fs::metadata(rp.join("file-alias")).unwrap().ino());assert_eq!(fs::read(rp.join("dest/moved/f000000")).unwrap(),b"inherited payload");remount.unmount().unwrap();
results.push(serde_json::json!({"entries":count,"prepare_ns":prepare.as_nanos(),"fork_ns":fork.as_nanos(),"rename_syscall_ns":rename.as_nanos(),"inodes_after_rename":3,"fork_samples_ns":fork_samples,"rename_samples_ns":rename_samples,"mmap_msync":true,"open_fd_unlink":true,"hardlink_restore":true,"raw_names":true,"atomic_save":true,"strict_busy_unmount":true}));}
eprintln!("phase=crash");let base=dir(&s,vec![f(&blob,b"file")]);m.prepare(&base).unwrap();let id=Uuid::new_v4().to_string();drop(m.fork(&id,&base,owner()).unwrap());let out=std::process::Command::new(std::env::current_exe().unwrap()).args(["crash",root.to_str().unwrap(),&id]).output().unwrap();assert!(!out.status.success());assert!(root.join("crash-ack").exists(),"{}",String::from_utf8_lossy(&out.stderr));let recovered=m.open_view(&id,&owner()).unwrap();let inode=recovered.lookup(1,b"acknowledged").unwrap().ino;assert!(recovered.read(inode,0,100).unwrap().starts_with(b"FUSE-fsynced"));
println!("{}",serde_json::json!({"schema":1,"backend":"rudder-logical-fuser-0.18.0","kernel":fs::read_to_string("/proc/sys/kernel/osrelease").unwrap().trim(),"writeback_cache":false,"scaling":results,"server_abort_recovery":true,"detached_descendant_busy_then_reaped":true,"uid":unsafe{libc::geteuid()},"uid_map":fs::read_to_string("/proc/self/uid_map").unwrap().trim(),"gid_map":fs::read_to_string("/proc/self/gid_map").unwrap().trim(),"active_host_modified":false}));}
"###;

#[test]
fn mounted_driver_compiles_without_touching_host_mounts() {
    use std::{fs, process::Command};
    let root = std::env::var_os("RUDDER_TEST_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            if std::path::Path::new("/workspace").is_dir() {
                "/workspace".into()
            } else {
                std::env::temp_dir()
            }
        });
    let tmp = tempfile::Builder::new()
        .prefix("rudder-vfs-driver-")
        .tempdir_in(root)
        .unwrap();
    let source = tmp.path().join("driver.rs");
    fs::write(&source, DRIVER).unwrap();
    let deps = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let library = |name: &str| {
        fs::read_dir(&deps)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("lib{name}-"))
                    && e.path().extension().is_some_and(|e| e == "rlib")
            })
            .max_by_key(|e| e.metadata().unwrap().modified().unwrap())
            .unwrap()
            .path()
    };
    let output = std::env::var_os("RUDDER_VFS_DRIVER_OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| tmp.path().join("driver"));
    let mut cmd = Command::new("rustc");
    cmd.arg("--edition=2021")
        .arg(&source)
        .arg("-L")
        .arg(format!("dependency={}", deps.display()));
    for name in ["rudder_engine", "libc", "serde_json", "uuid"] {
        cmd.arg("--extern")
            .arg(format!("{name}={}", library(name).display()));
    }
    let r = cmd.arg("-o").arg(&output).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    println!("mounted_driver={}", output.display());
}
