use super::*;
use std::os::unix::fs::DirBuilderExt;
#[test]
fn kernel_mount_identity_parser_rejects_missing_ambiguous_and_invalid_values() {
    assert_eq!(parse_mount_id(b"pos:\t0\nmnt_id:\t42\nino:\t7\n").unwrap(),42);
    for bytes in [b"ino:\t7\n".as_slice(),b"mnt_id: 0\n",b"mnt_id: -1\n",b"mnt_id: 1\nmnt_id: 1\n",b"mnt_id: 18446744073709551616\n"]{assert!(parse_mount_id(bytes).is_err());}
}
#[test]
fn actual_held_root_directory_has_live_procfs_mount_identity() {
    let root=File::open("/").unwrap();let id=mount_id(&root).unwrap();assert!(id>0);assert_eq!(mount_id(&root).unwrap(),id);
    // No Root config/volume owner is constructed from these DATA observations.
}
struct Fixture {root:PathBuf,parent:File}
impl Fixture {fn new()->Self {let root=std::env::temp_dir().join(format!("rudder-volume-init-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let parent=File::open(&root).unwrap();Self{root,parent}}}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.root);}}
#[test]
fn preexisting_directory_is_never_adopted_or_chowned_and_attempt_cannot_restart() {
    let f=Fixture::new();fs::create_dir(f.root.join("existing")).unwrap();fs::write(f.root.join("existing/history"),b"keep history").unwrap();let original=FinalizationBudget::begin(Duration::from_secs(2)).unwrap();
    let mut value=NewDirectory::stage(&f.parent,"existing",mount_id(&f.parent).unwrap()).unwrap();assert!(value.create_retaining(&original).is_err());assert!(!value.created_ack);assert!(value.held.is_none());assert!(!value.chown_attempted);assert_eq!(fs::read(f.root.join("existing/history")).unwrap(),b"keep history");
    if unsafe{libc::geteuid()}==0 {assert!(value.attempted);assert!(value.create_retaining(&original).is_err());}else{assert!(clock(&original).is_err());}
}
#[test]
fn actual_root_new_directory_handoff_refuses_unexpected_data_before_chown() {
    let f=Fixture::new();let original=FinalizationBudget::begin(Duration::from_secs(2)).unwrap();let mut value=NewDirectory::stage(&f.parent,"new",mount_id(&f.parent).unwrap()).unwrap();
    if unsafe{libc::geteuid()}!=0 {assert!(value.create_retaining(&original).is_err());assert!(!value.attempted);return;}
    value.create_retaining(&original).unwrap();let actual=value.id.unwrap();fs::write(f.root.join("new/unknown"),b"keep").unwrap();assert!(value.handoff_empty_retaining(&original).is_err());assert!(!value.chown_attempted);assert_eq!(inode(value.held.as_ref().unwrap()).unwrap(),actual);assert_eq!(fs::read(f.root.join("new/unknown")).unwrap(),b"keep");
}
#[test]
fn cancellation_before_birth_keeps_new_namespace_absent() {
    let f=Fixture::new();let original=FinalizationBudget::begin(Duration::from_secs(2)).unwrap();original.cancel();let mut value=NewDirectory::stage(&f.parent,"cancelled",mount_id(&f.parent).unwrap()).unwrap();
    assert!(value.create_retaining(&original).is_err());assert!(!value.attempted);assert!(!f.root.join("cancelled").try_exists().unwrap());
}
