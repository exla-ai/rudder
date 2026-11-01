//! Explicit root-only filesystem qualification; no mounts or user checkout.
//! Original held directories/pins are captured normally, not forged in tests.
use super::*;
use std::time::Duration;
struct Scratch(PathBuf);
impl Scratch {
    fn capture()->Self {
        assert_eq!(unsafe {libc::geteuid()},0,"run explicitly as root on disposable qualification host");
        let path=Path::new("/workspace").join(format!("rudder-retire-fs-{}",uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();Self(path)
    }
}
impl Drop for Scratch {fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
fn original()->FinalizationBudget {FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
#[test]
#[ignore="actual UID0 owned filesystem custody; explicit disposable root qualification"]
fn original_unlink_intent_reconciles_actual_absence_but_never_a_replacement_inode() {
    let scratch=Scratch::capture();let path=scratch.0.join("owned");
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();let held=HeldDir::root(&path,0o700).unwrap();
    fs::remove_dir(&path).unwrap();assert!(original_directory_present(&held,false).is_err());
    // Fault seam: exact original pre-effect intent was recorded, unlink really
    // succeeded, but its callback did not return. Same held inode survives.
    let mut attempted=true;remove_original_directory(&held,&mut attempted,&original()).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    assert!(remove_original_directory(&held,&mut attempted,&original()).is_err());assert!(path.is_dir());
}
#[test]
#[ignore="actual UID0 owned filesystem custody; explicit disposable root qualification"]
fn original_root_pin_reconciles_own_unpin_only_and_preserves_other_retention() {
    let scratch=Scratch::capture();let store=LocalStore::open(scratch.0.join("cas")).unwrap();
    let seed=store.put_directory(rudder_engine::store::DirectoryManifest {format_version:1,mode:0o700,entries:Vec::new()}).unwrap();
    store.pin("independent-original",&seed).unwrap();
    let mut lost=SourcePin::capture(store.clone(),seed.clone(),uuid::Uuid::new_v4()).unwrap();lost.acquire().unwrap();
    store.unpin(&lost.name).unwrap();assert!(lost.release().is_err());assert!(!lost.removed);
    let mut original=SourcePin::capture(store.clone(),seed.clone(),uuid::Uuid::new_v4()).unwrap();original.acquire().unwrap();
    original.unpin_attempted=true;store.unpin(&original.name).unwrap();original.release().unwrap();assert!(original.removed);
    assert_eq!(store.retention().unwrap().pins.get("independent-original"),Some(&seed));
}
