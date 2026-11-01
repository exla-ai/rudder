//! Actual held-file continuity and DATA schema tests. No synthetic config can
//! create ManagedHostRuntime; production guest/immutable-image proof is separate.
use super::*;
use std::os::unix::fs::{DirBuilderExt,PermissionsExt};
#[test]
fn manifest_has_exact_declared_schema_and_profile_data() {
    let bytes=br#"{"version":1,"profile_id":"fly-managed-image-v1","entries":[{"path":"/opt/rudder-managed-runtime/v1/codex","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size_bytes":4}]}"#;
    let parsed:Manifest=serde_json::from_slice(bytes).unwrap();assert_eq!(parsed.version,1);assert_eq!(parsed.entries[0].path,Path::new(ROOT).join("codex"));
    assert!(serde_json::from_slice::<Manifest>(br#"{"version":1,"profile_id":"fly-managed-image-v1","entries":[],"env":{"API_KEY":"never"}}"#).is_err());
}
#[test]
fn a_world_writable_ancestor_is_rejected_even_for_a_root_owned_readonly_leaf() {
    let root=std::env::temp_dir().join(format!("rudder-image-ancestor-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let file=root.join("immutable");fs::write(&file,b"image").unwrap();fs::set_permissions(&file,fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&root,fs::Permissions::from_mode(0o777)).unwrap();assert!(ancestors(&file).is_err());fs::set_permissions(&root,fs::Permissions::from_mode(0o700)).unwrap();fs::remove_dir_all(root).unwrap();
}
#[test]
fn actual_protected_system_file_retains_original_physical_inode() {
    // /usr/bin/env is a real protected system image file. Unsupported/non-root
    // writable containers must explicitly refuse rather than minting custody.
    let path=Path::new("/usr/bin/env");if !path.try_exists().unwrap_or(false){assert!(stamp(path).is_err());return;}
    let original=FinalizationBudget::begin(std::time::Duration::from_secs(2)).unwrap();let cancelled=AtomicBool::new(false);
    match ImageFile::open(path,1024*1024,&original,&cancelled) {
        Ok(held)=>{held.current().unwrap();assert_eq!(held.stamp.dev,held.held.metadata().unwrap().dev());assert_eq!(held.stamp.ino,held.held.metadata().unwrap().ino());},
        Err(_)=>assert!(ancestors(path).is_err()||stamp(path).is_err(),"actual protected image capture failed unexpectedly"),
    }
    // Read-only real-system continuity check; this does not mutate /usr or
    // stand in for the complete model image/guest qualification.
}
