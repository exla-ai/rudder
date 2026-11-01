//! Ordinary-UID bounded file DATA tests. They never construct RuntimeImageClient,
//! registered engine/config, machine CAS, environment ACK or compiler authority.
use super::*;
struct TempFixture(PathBuf);
impl TempFixture {
    fn new()->Self {
        let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};
        let path=base.join(format!("rudder-backed-test-{}",uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();Self(path)
    }
    fn path(&self)->&Path{&self.0}
}
impl Drop for TempFixture {fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}

use std::os::unix::fs::PermissionsExt;
fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn cancel()->AtomicBool{AtomicBool::new(false)}
fn fixture()->(TempFixture,PathBuf){
    let temp=TempFixture::new();let path=temp.path().canonicalize().unwrap().join("public-network");
    fs::write(&path,b"127.0.0.1 component.local\n").unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o600)).unwrap();(temp,path)
}
fn uid()->(u32,u32){(unsafe{libc::geteuid()},unsafe{libc::getegid()})}

#[test]
fn backed_network_actual_ordinary_uid_data_read_has_exact_bytes_and_retained_fd(){
    let (_temp,path)=fixture();let mut owner=NetworkFile::staged(path.clone());owner.capture_retaining(&budget(),&cancel(),uid()).unwrap();
    assert_eq!(owner.bytes,b"127.0.0.1 component.local\n");assert!(owner.file.is_some());owner.current().unwrap();
    let moved=path.with_extension("old");fs::rename(&path,&moved).unwrap();fs::write(&path,b"different").unwrap();
    assert!(owner.current().is_err());assert_eq!(owner.file.as_ref().unwrap().metadata().unwrap().len(),26);
}

#[test]
fn backed_network_oversize_open_keeps_original_handle_and_cannot_retry(){
    let (_temp,path)=fixture();OpenOptions::new().write(true).open(&path).unwrap().set_len(1024*1024+1).unwrap();
    let mut owner=NetworkFile::staged(path);assert!(owner.capture_retaining(&budget(),&cancel(),uid()).is_err());
    assert!(owner.file.is_some()&&owner.saved.is_some());assert!(owner.bytes.is_empty()&&!owner.ready);
    assert!(owner.capture_retaining(&budget(),&cancel(),uid()).is_err());assert!(owner.current().is_err());
}

#[test]
fn backed_network_writable_or_symlink_input_refuses_with_partial_custody(){
    let (_temp,path)=fixture();fs::set_permissions(&path,fs::Permissions::from_mode(0o666)).unwrap();
    let mut writable=NetworkFile::staged(path.clone());assert!(writable.capture_retaining(&budget(),&cancel(),uid()).is_err());
    assert!(writable.file.is_some()&&!writable.ready);let alias=path.with_extension("link");std::os::unix::fs::symlink(&path,&alias).unwrap();
    let mut link=NetworkFile::staged(alias);assert!(link.capture_retaining(&budget(),&cancel(),uid()).is_err());
    assert!(!link.ready&&link.file.is_none()&&!link.parents.is_empty());
}

#[test]
fn backed_network_original_cancel_stages_attempt_but_opens_no_file_and_no_new_clock(){
    let (_temp,path)=fixture();let original=budget();let constraint=original.constraint();original.cancel();
    let mut owner=NetworkFile::staged(path);assert!(owner.capture_retaining(&original,&cancel(),uid()).is_err());
    assert!(owner.attempted&&!owner.ready&&owner.file.is_none());assert_eq!(original.constraint(),constraint);
    assert!(owner.capture_retaining(&budget(),&cancel(),uid()).is_err());
}

#[test]
fn backed_network_changed_original_bytes_refuse_even_when_name_inode_is_unchanged(){
    let (_temp,path)=fixture();let mut owner=NetworkFile::staged(path.clone());owner.capture_retaining(&budget(),&cancel(),uid()).unwrap();
    let saved=owner.saved.as_ref().unwrap().clone();fs::write(&path,b"127.0.0.1 different.node\n").unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(),saved.ino);assert!(owner.current().is_err());assert!(owner.file.is_some());
}

#[test]
fn backed_network_incomplete_set_never_claims_current_environment_or_runtime_ready(){
    let empty=NetworkInputs::staged();assert!(empty.current().is_err());
    assert_eq!(NETWORK_PATHS,["/etc/hosts","/etc/resolv.conf","/etc/nsswitch.conf"]);
    let (_temp,path)=fixture();let partial=NetworkInputs{files:vec![NetworkFile::staged(path)],attempted:true};
    assert!(partial.current().is_err());
}
