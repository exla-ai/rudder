//! Actual tempdirs/FDs; test-only expectations use actual UID/GID, never Root grants.
use super::*;
use std::os::{fd::AsRawFd,unix::fs::{DirBuilderExt,PermissionsExt,symlink}};
struct TempDirectory(PathBuf);
impl TempDirectory {fn path(&self)->&Path{&self.0}}
impl Drop for TempDirectory {fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
fn fixture()->TempDirectory{let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};let path=base.join(format!("rudder-directory-custody-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&path).unwrap();TempDirectory(path)}
fn owner()->DirectoryAnchors{DirectoryAnchors{uid:unsafe{libc::getuid()},gid:unsafe{libc::getgid()},..Default::default()}}
#[test]
fn original_anchor_deduplicates_exact_path_without_recursive_parent_handles(){let t=fixture();let mut p=owner();let a=p.capture(t.path()).unwrap();let b=p.capture(t.path()).unwrap();
    assert!(Arc::ptr_eq(&a,&b));assert_eq!(p.paths.len(),1);let fd=a.held.as_raw_fd();let flags=unsafe{libc::fcntl(fd,libc::F_GETFL)};
    assert_eq!(flags&libc::O_PATH,libc::O_PATH);assert_eq!(flags&libc::O_DIRECTORY,libc::O_DIRECTORY);assert_eq!(flags&libc::O_NOFOLLOW,libc::O_NOFOLLOW);assert_ne!(unsafe{libc::fcntl(fd,libc::F_GETFD)}&libc::FD_CLOEXEC,0);
    drop(p);a.current().unwrap();b.current().unwrap();}
#[test]
fn actual_replacement_cannot_adopt_a_new_name_or_inode(){let t=fixture();let path=t.path().join("directory");fs::DirBuilder::new().mode(0o700).create(&path).unwrap();let mut p=owner();let a=p.capture(&path).unwrap();
    fs::rename(&path,t.path().join("moved")).unwrap();fs::DirBuilder::new().mode(0o700).create(&path).unwrap();assert!(a.current().is_err());assert!(p.capture(&path).is_err());assert_eq!(a.held.metadata().unwrap().ino(),a.stamp.ino);}
#[test]
fn actual_mode_change_and_symlink_refuse(){let t=fixture();let path=t.path().join("directory");fs::DirBuilder::new().mode(0o700).create(&path).unwrap();let mut p=owner();let a=p.capture(&path).unwrap();
    fs::set_permissions(&path,fs::Permissions::from_mode(0o755)).unwrap();assert!(a.current().is_err());fs::rename(&path,t.path().join("old")).unwrap();symlink("old",&path).unwrap();assert!(p.capture(&path).is_err());assert!(owner().capture(&path).is_err());}
#[test]
fn error_and_unwind_leave_original_partial_fd_owned_and_nonreplayable(){let t=fixture();let mut p=owner();assert!(p.capture_inner(t.path(),||anyhow::bail!("after-open fault")).is_err());
    assert!(p.paths[t.path()].held.is_some());assert!(p.capture(t.path()).is_err());let other=fixture();let mut q=owner();assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{let _=q.capture_inner(other.path(),||panic!("after-open unwind"));})).is_err());assert!(q.paths[other.path()].held.is_some());assert!(q.capture(other.path()).is_err());}
#[test]
fn explicit_capacity_and_owner_mismatch_refuse_without_dropping_existing_anchor(){let t=fixture();let path=t.path().join("second");fs::DirBuilder::new().mode(0o700).create(&path).unwrap();let mut p=owner();p.limit=1;let a=p.capture(t.path()).unwrap();
    assert!(p.capture(&path).is_err());a.current().unwrap();let mut wrong=owner();wrong.uid=wrong.uid.wrapping_add(1);assert!(wrong.capture(&path).is_err());assert!(wrong.paths[&path].held.is_none());}
