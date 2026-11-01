//! Actual Linux file/FD tests with a current-UID private tiny fixture. These do NOT
//! create a Root bootstrap, credential, namespace or readiness grant. UNRUN.
use super::*;
use std::{io::Write,os::unix::fs::{MetadataExt,PermissionsExt,FileExt},path::PathBuf,time::Duration};
struct Fixture {path:PathBuf}
impl Fixture {
    fn new(mode:u32)->Self {
        let path=std::env::temp_dir().join(format!("rudder-host-file-mode-test-{}-{}",std::process::id(),uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o700)).unwrap();
        for (name,data) in [("config.json",b"{}".to_vec()),("token",vec![b'x';32]),("registration-key",[vec![b'0';64],vec![b'\n']].concat())] {
            let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).open(path.join(name)).unwrap();file.write_all(&data).unwrap();file.sync_all().unwrap();
            fs::set_permissions(path.join(name),fs::Permissions::from_mode(mode)).unwrap();}
        Self {path}
    }
    fn mode(&self,name:&str)->u32 {fs::symlink_metadata(self.path.join(name)).unwrap().mode()&0o7777}
    fn cleanup(self) {fs::remove_dir_all(self.path).unwrap();} // exact owned test fixture only
}
fn budget()->FinalizationBudget {FinalizationBudget::begin(Duration::from_secs(10)).unwrap()}
fn fixture_prepare(out:&mut Option<ProvisionedFileModes>,fixture:&Fixture,original:&FinalizationBudget,hook:Option<fn(&FinalizationBudget)>)->Result<()> {
    // Test-only current-UID DATA path. Production has ONLY the fixed Root path.
    ensure!(out.is_none(),"fixture already staged");*out=Some(ProvisionedFileModes::empty(original,(unsafe {libc::geteuid()},unsafe {libc::getegid()})));
    let owner=out.as_mut().unwrap();owner.after_chmod=hook;owner.original.remaining()?;
    if owner.absent_or_complete(&fixture.path)? {owner.phase=Phase::Absent;return Ok(());}
    owner.open_directory(None,fixture.path.to_str().unwrap(),true)?;owner.capture_and_normalize()
}
#[test]
fn observed755_originals_become600_with_same_bytes_original_fds_and_cloexec() {
    let fixture=Fixture::new(0o755);let original=budget();let mut out=None;fixture_prepare(&mut out,&fixture,&original,None).unwrap();let owner=out.as_ref().unwrap();
    assert!(owner.configured().unwrap());assert_eq!(owner.files.len(),3);owner.check_current().unwrap();
    for slot in &owner.files {assert!(slot.chmod_attempted&&slot.sync_attempted);let before=slot.before.unwrap();let after=slot.after.unwrap();assert!(before.same_payload_metadata(&after));assert_eq!(after.mode&0o7777,0o600);
        assert_ne!(unsafe {libc::fcntl(slot.file.as_raw_fd(),libc::F_GETFD)}&libc::FD_CLOEXEC,0);let mut bytes=vec![0;after.len as usize];assert_eq!(slot.file.read_at(&mut bytes,0).unwrap(),bytes.len());assert_eq!(bytes,fs::read(fixture.path.join(slot.spec.name)).unwrap());}
    for directory in &owner.directories {assert_ne!(unsafe {libc::fcntl(directory.file.as_raw_fd(),libc::F_GETFD)}&libc::FD_CLOEXEC,0);}
    drop(out);fixture.cleanup();
}
#[test]
fn exact600_has_no_chmod_or_stamp_change_and_absent_profile_stays_absent() {
    let fixture=Fixture::new(0o600);let mut out=None;fixture_prepare(&mut out,&fixture,&budget(),None).unwrap();
    for slot in &out.as_ref().unwrap().files {assert!(!slot.chmod_attempted&&!slot.sync_attempted);assert_eq!(slot.before,slot.after);}
    drop(out);for spec in FILES {fs::remove_file(fixture.path.join(spec.name)).unwrap();}
    let mut absent=None;fixture_prepare(&mut absent,&fixture,&budget(),None).unwrap();let owner=absent.as_ref().unwrap();assert!(!owner.configured().unwrap());assert!(owner.files.is_empty()&&owner.directories.is_empty());drop(absent);fixture.cleanup();
}
#[test]
fn incomplete_or_invalid_sizes_refuse_before_any_original_mode_mutation() {
    let fixture=Fixture::new(0o755);fs::remove_file(fixture.path.join("registration-key")).unwrap();let mut out=None;
    assert!(fixture_prepare(&mut out,&fixture,&budget(),None).is_err());assert!(out.as_ref().unwrap().files.is_empty());assert_eq!(fixture.mode("config.json"),0o755);drop(out);
    let mut key=OpenOptions::new().write(true).create_new(true).mode(0o755).open(fixture.path.join("registration-key")).unwrap();key.write_all(&[b'0';64]).unwrap();drop(key);
    let mut out=None;assert!(fixture_prepare(&mut out,&fixture,&budget(),None).is_err());let owner=out.as_ref().unwrap();assert_eq!(owner.files.len(),3);assert!(owner.files.iter().all(|f|!f.chmod_attempted));
    assert_eq!(fixture.mode("config.json"),0o755);assert_eq!(fixture.mode("token"),0o755);drop(out);fixture.cleanup();
}
#[test]
fn unsupported_mode_symlink_and_hardlink_are_never_repaired() {
    let fixture=Fixture::new(0o755);fs::set_permissions(fixture.path.join("config.json"),fs::Permissions::from_mode(0o644)).unwrap();let mut out=None;
    assert!(fixture_prepare(&mut out,&fixture,&budget(),None).is_err());assert_eq!(fixture.mode("config.json"),0o644);assert_eq!(fixture.mode("token"),0o755);drop(out);
    fs::set_permissions(fixture.path.join("config.json"),fs::Permissions::from_mode(0o755)).unwrap();fs::hard_link(fixture.path.join("token"),fixture.path.join("token-alias")).unwrap();let mut out=None;
    assert!(fixture_prepare(&mut out,&fixture,&budget(),None).is_err());assert_eq!(fixture.mode("config.json"),0o755);drop(out);fs::remove_file(fixture.path.join("token-alias")).unwrap();
    fs::rename(fixture.path.join("config.json"),fixture.path.join("config-original")).unwrap();std::os::unix::fs::symlink("config-original",fixture.path.join("config.json")).unwrap();let mut out=None;
    assert!(fixture_prepare(&mut out,&fixture,&budget(),None).is_err());assert_eq!(fixture.mode("config-original"),0o755);drop(out);fixture.cleanup();
}
#[test]
fn parent_mode_and_cancel_before_effects_refuse_without_normalizing_files() {
    let fixture=Fixture::new(0o755);fs::set_permissions(&fixture.path,fs::Permissions::from_mode(0o755)).unwrap();let mut out=None;
    assert!(fixture_prepare(&mut out,&fixture,&budget(),None).is_err());assert_eq!(fixture.mode("config.json"),0o755);drop(out);fs::set_permissions(&fixture.path,fs::Permissions::from_mode(0o700)).unwrap();
    let original=budget();original.cancel();let mut out=None;assert!(fixture_prepare(&mut out,&fixture,&original,None).is_err());assert!(out.is_some());assert_eq!(fixture.mode("config.json"),0o755);drop(out);fixture.cleanup();
}
#[test]
fn cancellation_after_actual_chmod_keeps_all_originals_and_cannot_retry_or_ack() {
    fn cancel(original:&FinalizationBudget){original.cancel();}
    let fixture=Fixture::new(0o755);let original=budget();let mut out=None;assert!(fixture_prepare(&mut out,&fixture,&original,Some(cancel)).is_err());let owner=out.as_ref().unwrap();
    assert_eq!(owner.files.len(),3);assert_eq!(fixture.mode("config.json"),0o600);assert_eq!(fixture.mode("token"),0o755);assert!(owner.files[0].chmod_attempted&&!owner.files[0].sync_attempted);
    assert!(owner.check_current().is_err());assert!(owner.files.iter().all(|f|unsafe {libc::fcntl(f.file.as_raw_fd(),libc::F_GETFD)}>=0));
    assert!(fixture_prepare(&mut out,&fixture,&budget(),None).is_err());drop(out);fixture.cleanup();
}
#[test]
fn panic_after_real_chmod_retains_actual_carrier_outside_unwind() {
    fn fail(_: &FinalizationBudget){panic!("TEST_ONLY_AFTER_ACTUAL_CHMOD");}
    let fixture=Fixture::new(0o755);let mut out=None;
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||fixture_prepare(&mut out,&fixture,&budget(),Some(fail)))).is_err());
    let owner=out.as_ref().unwrap();assert_eq!(owner.files.len(),3);assert!(owner.files[0].chmod_attempted);assert_eq!(fixture.mode("config.json"),0o600);assert!(owner.configured().is_err());
    assert!(owner.files.iter().all(|f|unsafe {libc::fcntl(f.file.as_raw_fd(),libc::F_GETFD)}>=0));drop(out);fixture.cleanup();
}
#[test]
fn held_identity_detects_replacement_while_completed_protection_is_not_a_renewed_clock() {
    let fixture=Fixture::new(0o755);let original=budget();let mut out=None;fixture_prepare(&mut out,&fixture,&original,None).unwrap();original.cancel();out.as_ref().unwrap().check_current().unwrap();
    fs::rename(fixture.path.join("config.json"),fixture.path.join("config-old")).unwrap();let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).open(fixture.path.join("config.json")).unwrap();file.write_all(b"{}").unwrap();drop(file);
    assert!(out.as_ref().unwrap().check_current().is_err());assert!(unsafe {libc::fcntl(out.as_ref().unwrap().files[0].file.as_raw_fd(),libc::F_GETFD)}>=0);drop(out);fixture.cleanup();
}
