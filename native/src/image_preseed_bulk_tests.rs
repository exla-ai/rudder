//! Actual file/CAS mechanisms and genuine original ImagePreseedBudget only.
//! These non-Root fixtures do not qualify fixed image/source/kernel authority.
use super::*;
use std::os::unix::fs::PermissionsExt;
struct Fixture {store:LocalStore,path:PathBuf}
impl Fixture {fn new()->Self {let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};
    let path=base.join(format!("rudder-image-bulk-test-{}",uuid::Uuid::new_v4()));fs::create_dir(&path).unwrap();let store=LocalStore::open(path.join("cas")).unwrap();Self{store,path}}}
impl Drop for Fixture {fn drop(&mut self){let _=fs::remove_dir_all(&self.path);}}
fn captured(path:&Path)->Stamp {let m=fs::symlink_metadata(path).unwrap();Stamp{path:path.into(),dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())}}
fn stage(f:&Fixture,original:&ImagePreseedBudget,cancelled:Arc<AtomicBool>)->LocalImageImport {
    let mut out=None;LocalImageImport::stage_retaining(&mut out,f.store.clone(),Arc::new(ImportClock{original:original.clone(),cancelled})).unwrap();out.unwrap()
}

#[test]
fn image_bulk_actual_paged_file_and_hardlinks_match_ordinary_bytes_without_early_ack() {
    let f=Fixture::new();let path=f.path.join("large");let bytes=vec![0x7b;content::CHUNK_BYTES+31];fs::write(&path,&bytes).unwrap();
    fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();let alias=f.path.join("alias");fs::hard_link(&path,&alias).unwrap();
    let original=ImagePreseedBudget::begin(Duration::from_secs(10)).unwrap();let cancelled=Arc::new(AtomicBool::new(false));let import=stage(&f,&original,cancelled.clone());
    let mut files=ImportedFiles::default();let first=files.read_with(File::open(&path).unwrap(),&captured(&path),&import,&original,&cancelled).unwrap();
    assert_eq!(first.0.kind,ObjectKind::File);assert_eq!(first.1,format!("{:x}",Sha256::digest(&bytes)));
    assert_eq!(files.read_with(File::open(&alias).unwrap(),&captured(&alias),&import,&original,&cancelled).unwrap(),first);
    assert_eq!(files.bytes,bytes.len()as u64);assert_eq!(files.inodes.len(),1);assert_eq!(import.stats_data().unwrap().object_file_syncs,0);
    let installer=PagedDirectory::new(&import).build(0o555,[DirectoryEntry::new(b"large",EntryNode::File{object:first.0.clone(),logical_size:bytes.len()as u64,mode:0o444}).unwrap()]).unwrap();
    let manifest_bytes=canonical_json(&ImageManifestData{format_version:1,installer_input:installer.clone(),files:vec![ImageFileData{path:path.clone(),sha256:first.1,size_bytes:bytes.len()as u64,content:first.0.clone()}]}).unwrap();
    let manifest=ObjectRef::from_bytes(ObjectKind::Blob,&manifest_bytes);import.receive_manifest(&manifest,&manifest_bytes).unwrap();
    let ack=import.commit_pins("image",&installer,"manifest",&manifest).unwrap();import.assert_pins_current(&ack).unwrap();
    assert_eq!(content::read_file_range(&f.store,&first.0,content::CHUNK_BYTES as u64-8,39).unwrap(),bytes[content::CHUNK_BYTES-8..]);
    assert!(import.objects_after_commit(&ack).unwrap().contains(&first.0));
}

#[test]
fn image_bulk_same_original_cancellation_refuses_pin_and_retains_actual_hold() {
    let f=Fixture::new();let original=ImagePreseedBudget::begin(Duration::from_secs(10)).unwrap();let cancelled=Arc::new(AtomicBool::new(false));
    let import=stage(&f,&original,cancelled.clone());let installer=empty_provider_directory_to(&import).unwrap();
    let manifest=ObjectRef::from_bytes(ObjectKind::Blob,b"manifest");import.receive_manifest(&manifest,b"manifest").unwrap();
    cancelled.store(true,Ordering::Release);
    assert!(import.commit_pins("image",&installer,"manifest",&manifest).is_err());assert!(original.remaining().is_err());
    assert!(f.store.retention().unwrap().pins.is_empty());assert_eq!(f.store.retention().unwrap().image_import_holds.len(),1);
}

#[test]
fn image_bulk_provider_stubs_keep_exact_immutable_manifest_identity() {
    let f=Fixture::new();let original=ImagePreseedBudget::begin(Duration::from_secs(10)).unwrap();let import=stage(&f,&original,Arc::new(AtomicBool::new(false)));
    let actual_directory=empty_provider_directory_to(&import).unwrap();let actual_file=empty_provider_file_to(&import).unwrap();
    let other=LocalStore::open(f.path.join("ordinary")).unwrap();let lease=other.begin_staging(Duration::from_secs(30)).unwrap();
    assert_eq!(actual_directory,empty_provider_directory(&other,&lease.id).unwrap());
    assert_eq!(actual_file,empty_provider_file(&other,&lease.id).unwrap());
    assert_eq!(import.stats_data().unwrap().object_file_syncs,0);
}
