//! Actual temporary dirs and Linux FDs. Test-only owner expectations use the
//! actual test UID/GID; no production Root/profile capability is constructed.
use super::*;
use std::os::{fd::AsRawFd,unix::fs::{DirBuilderExt,PermissionsExt,symlink}};
struct TempDirectory(PathBuf);
impl TempDirectory {fn path(&self)->&Path{&self.0}}
impl Drop for TempDirectory {fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
fn fixture()->TempDirectory{let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};let path=base.join(format!("rudder-directory-custody-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&path).unwrap();TempDirectory(path)}
fn test_owner()->DirectoryCustody{DirectoryCustody{uid:unsafe{libc::getuid()},gid:unsafe{libc::getgid()},..Default::default()}}
#[test]
fn actual_path_descriptor_is_nofollow_cloexec_and_exact_path_deduplicated(){
    let t=fixture();let mut owner=test_owner();let before=owner.capture(t.path()).unwrap();let first=owner.held[t.path()].file.as_ref().unwrap().as_raw_fd();
    let flags=unsafe{libc::fcntl(first,libc::F_GETFL)};assert_eq!(flags&libc::O_PATH,libc::O_PATH);assert_eq!(flags&libc::O_DIRECTORY,libc::O_DIRECTORY);assert_eq!(flags&libc::O_NOFOLLOW,libc::O_NOFOLLOW);
    assert_ne!(unsafe{libc::fcntl(first,libc::F_GETFD)}&libc::FD_CLOEXEC,0);assert!(owner.capture(t.path()).unwrap()==before);assert_eq!(owner.held.len(),1);assert_eq!(owner.held[t.path()].file.as_ref().unwrap().as_raw_fd(),first);owner.check_all_with(||Ok(())).unwrap();
}
#[test]
fn real_directory_replacement_retains_original_fd_and_refuses_name_adoption(){
    let t=fixture();let p=t.path().join("directory");fs::DirBuilder::new().mode(0o700).create(&p).unwrap();let mut owner=test_owner();let before=owner.capture(&p).unwrap();
    let moved=t.path().join("moved");fs::rename(&p,&moved).unwrap();fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
    assert!(owner.check_path(&p,&before).is_err());assert!(owner.capture(&p).is_err());assert_eq!(owner.held[&p].file.as_ref().unwrap().metadata().unwrap().ino(),before.ino);
}
#[test]
fn post_open_failure_keeps_actual_fd_and_incomplete_slot_without_retry(){
    let t=fixture();let p=t.path().join("directory");fs::DirBuilder::new().mode(0o700).create(&p).unwrap();let mut owner=test_owner();
    assert!(owner.capture_inner(&p,||anyhow::bail!("test after-open failure")).is_err());assert!(owner.held[&p].file.is_some());assert!(!owner.held[&p].complete);assert!(owner.capture(&p).is_err());
}
#[test]
fn callback_unwind_keeps_original_descriptor_in_outer_owner(){
    let t=fixture();let mut owner=test_owner();let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{let _=owner.capture_inner(t.path(),||panic!("test unwind"));}));
    assert!(result.is_err());assert!(owner.held[t.path()].file.is_some());assert!(!owner.held[t.path()].complete);assert!(owner.capture(t.path()).is_err());
}
#[test]
fn actual_mode_mutation_and_symlink_replacement_refuse(){
    let t=fixture();let p=t.path().join("directory");fs::DirBuilder::new().mode(0o700).create(&p).unwrap();let mut owner=test_owner();let before=owner.capture(&p).unwrap();
    fs::set_permissions(&p,fs::Permissions::from_mode(0o755)).unwrap();assert!(owner.check_path(&p,&before).is_err());fs::rename(&p,t.path().join("old")).unwrap();symlink("old",&p).unwrap();assert!(owner.capture(&p).is_err());
    let mut fresh=test_owner();assert!(fresh.capture(&p).is_err());assert!(fresh.held[&p].file.is_none());
}
#[test]
fn path_and_descriptor_count_are_bounded_without_dropping_existing_owner(){
    let t=fixture();let a=t.path().join("a");let b=t.path().join("b");for p in [&a,&b]{fs::DirBuilder::new().mode(0o700).create(p).unwrap();}
    let mut owner=test_owner();owner.limit=1;let first=owner.capture(&a).unwrap();assert!(owner.capture(&b).is_err());assert_eq!(owner.held.len(),1);owner.check_path(&a,&first).unwrap();
    assert!(owner.capture(Path::new("relative")).is_err());assert!(owner.capture(&t.path().join("..")).is_err());assert!(owner.capture(&Path::new("/").join("a".repeat(MAX_PATH_BYTES+1))).is_err());
}
#[test]
fn real_owner_mismatch_is_refused_without_fabricated_root_fixture(){
    let t=fixture();let mut owner=test_owner();owner.uid=owner.uid.wrapping_add(1);assert!(owner.capture(t.path()).is_err());assert!(owner.held[t.path()].file.is_none());
}
