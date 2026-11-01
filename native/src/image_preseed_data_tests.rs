use super::*;
use std::os::unix::fs::{DirBuilderExt,PermissionsExt};
struct Fixture {path:PathBuf,store:LocalStore}
impl Fixture {fn new()->Self {let path=std::env::temp_dir().join(format!("rudder-image-data-test-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&path).unwrap();let store=LocalStore::open(path.join("cas")).unwrap();Self{path,store}}}
impl Drop for Fixture {fn drop(&mut self){let _=fs::remove_dir_all(&self.path);}}
fn budget()->ImagePreseedBudget {ImagePreseedBudget::begin(Duration::from_secs(10)).unwrap()}
#[test]
fn full_manifest_is_digest_neutral_canonical_and_includes_actual_installer_ref() {
    let f=Fixture::new();let installer=f.store.put_directory(rudder_engine::store::DirectoryManifest{format_version:1,mode:0o555,entries:vec![]}).unwrap();
    let data=ImageManifestData{format_version:1,installer_input:installer.clone(),files:vec![]};let raw=canonical_json(&data).unwrap();
    assert!(!raw.ends_with(b"\n"));let value:serde_json::Value=serde_json::from_slice(&raw).unwrap();assert_eq!(value.as_object().unwrap().len(),3);assert!(value.get("image").is_none());assert!(value.get("image_receipt_sha256").is_none());
    assert_eq!(canonical_json(&serde_json::from_slice::<ImageManifestData>(&raw).unwrap()).unwrap(),raw);
    let reference=ObjectRef::from_bytes(ObjectKind::Blob,&raw);assert_eq!(f.store.put_blob(&raw).unwrap(),reference);let pin=ImagePreseedPinData{format_version:1,manifest:reference,installer_input:installer};assert_eq!(serde_json::from_slice::<ImagePreseedPinData>(&canonical_json(&pin).unwrap()).unwrap(),pin);
}
#[test]
fn only_exact_separate_manifest_gets_larger_than_chunk_allowance() {
    let f=Fixture::new();let bytes=vec![b'x';content::CHUNK_BYTES+1];let large=f.store.put_blob(&bytes).unwrap();
    let empty=f.store.put_directory(rudder_engine::store::DirectoryManifest{format_version:1,mode:0o555,entries:vec![]}).unwrap();
    f.store.pin("fixture-empty",&empty).unwrap();f.store.pin("fixture-large-manifest",&large).unwrap();
    assert!(graph_objects(&f.store,&empty,&large).unwrap().contains(&large));
    let ordinary=f.store.put_directory(rudder_engine::store::DirectoryManifest{format_version:1,mode:0o555,entries:vec![rudder_engine::store::DirectoryEntry::new(b"ordinary",rudder_engine::store::EntryNode::File{object:large,mode:0o444}).unwrap()]}).unwrap();
    f.store.pin("fixture-ordinary",&ordinary).unwrap();let manifest=f.store.put_blob(b"separate manifest data").unwrap();f.store.pin("fixture-manifest",&manifest).unwrap();assert!(graph_objects(&f.store,&ordinary,&manifest).is_err());
}
#[test]
fn actual_file_crossing_chunk_boundary_has_bounded_file_node_and_exact_whole_digest() {
    let f=Fixture::new();let path=f.path.join("content");let expected=vec![0x7bu8;content::CHUNK_BYTES+31];fs::write(&path,&expected).unwrap();
    let original=budget();let cancelled=AtomicBool::new(false);let stage=f.store.begin_staging(Duration::from_secs(30)).unwrap();let content_store=StagedContentStore::new(&f.store,&stage.id).unwrap();
    let mut reader=HashReader{file:File::open(&path).unwrap(),offset:0,length:expected.len() as u64,sha:Sha256::new(),original:&original,cancelled:&cancelled};let reference=content::import_file(&content_store,&mut reader).unwrap();
    assert_eq!(reference.kind,ObjectKind::File);assert_eq!(reader.offset,expected.len() as u64);assert_eq!(format!("{:x}",reader.sha.finalize()),format!("{:x}",Sha256::digest(&expected)));
    assert_eq!(content::read_file_range(&content_store,&reference,content::CHUNK_BYTES as u64-8,39).unwrap(),expected[content::CHUNK_BYTES-8..]);
    f.store.pin("fixture-file-node",&reference).unwrap();for item in f.store.retained_closure(&reference).unwrap(){assert!(item.size_bytes<=content::CHUNK_BYTES as u64);}
}
#[test]
fn actual_reader_cancellation_never_renews_original_budget_or_reads_more_bytes() {
    let f=Fixture::new();let path=f.path.join("content");fs::write(&path,[7u8;64]).unwrap();let original=budget();let cancelled=AtomicBool::new(false);
    let mut reader=HashReader{file:File::open(path).unwrap(),offset:0,length:64,sha:Sha256::new(),original:&original,cancelled:&cancelled};let mut bytes=[0u8;8];assert_eq!(reader.read(&mut bytes).unwrap(),8);
    cancelled.store(true,Ordering::Release);assert!(reader.read(&mut bytes).is_err());assert_eq!(reader.offset,8);assert!(original.remaining().is_err());
}
#[test]
fn actual_private_regular_file_is_rejected_before_copy_and_undeclared_symlink_escape_is_refused() {
    let f=Fixture::new();let path=f.path.join("private");fs::write(&path,b"private bytes").unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o600)).unwrap();
    if unsafe{libc::geteuid()}==0 {let captured=stamp(&path).unwrap();assert!(regular_readable(&captured).is_err());fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();assert!(regular_readable(&stamp(&path).unwrap()).is_ok());}
    else {assert!(stamp(&path).is_err());} // no non-Root fixture is an image proof
    let link=f.path.join("escape");std::os::unix::fs::symlink(&path,&link).unwrap();assert!(link_allowed(&link).is_err());
}
#[test]
fn merged_usr_and_alternatives_paths_remain_explicit_installer_topology() {
    assert_eq!(installer_path(Path::new("/usr/share/example")).unwrap(),vec![hex_bytes(b"usr"),hex_bytes(b"share"),hex_bytes(b"example")]);
    assert_eq!(installer_path(Path::new("/etc/alternatives/editor")).unwrap(),vec![hex_bytes(b"alternatives"),hex_bytes(b"editor")]);
    assert_eq!(installer_path(Path::new("/etc/ssl/openssl.cnf")).unwrap(),vec![hex_bytes(b"ssl-config")]);
    assert_eq!(installer_path(Path::new("/etc/python3.11/sitecustomize.py")).unwrap(),vec![hex_bytes(b"python-site")]);
    assert!(installer_path(Path::new("/etc/shadow")).is_err());assert!(installer_path(Path::new("/etc/ssl/private/key")).is_err());
}

#[test]
fn actual_file_growth_is_rejected_without_copying_beyond_original_length() {
    let f=Fixture::new();let path=f.path.join("growing");fs::write(&path,b"bounded").unwrap();let original=budget();let cancelled=AtomicBool::new(false);
    let mut reader=HashReader{file:File::open(&path).unwrap(),offset:0,length:7,sha:Sha256::new(),original:&original,cancelled:&cancelled};fs::write(&path,b"bounded-extra").unwrap();let mut out=[0u8;64];assert_eq!(reader.read(&mut out).unwrap(),7);assert!(reader.read(&mut out).is_err());assert_eq!(reader.offset,7);
}

#[test]
fn actual_hardlink_aliases_share_one_import_and_changed_inode_is_refused() {
    let f=Fixture::new();let path=f.path.join("git");let bytes=vec![9u8;64*1024];fs::write(&path,&bytes).unwrap();
    let aliases=(0..150).map(|i|{let p=f.path.join(format!("git-{i}"));fs::hard_link(&path,&p).unwrap();p}).collect::<Vec<_>>();
    // This exercises content accounting DATA with real descriptors and inodes.
    // Root ownership remains the separate outer stamp/capture admission gate.
    let captured=|path:&Path|{let m=fs::symlink_metadata(path).unwrap();Stamp{path:path.into(),dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())}};
    let stage=f.store.begin_staging(Duration::from_secs(60)).unwrap();let mut imported=ImportedFiles::default();let original=budget();let cancelled=AtomicBool::new(false);
    let first=imported.read(File::open(&path).unwrap(),&captured(&path),&f.store,&stage.id,&original,&cancelled).unwrap();
    for alias in &aliases {assert_eq!(imported.read(File::open(alias).unwrap(),&captured(alias),&f.store,&stage.id,&original,&cancelled).unwrap(),first);}
    assert_eq!(imported.bytes,bytes.len()as u64);assert_eq!(imported.inodes.len(),1);
    fs::write(&path,b"changed").unwrap();assert!(imported.read(File::open(&aliases[0]).unwrap(),&captured(&aliases[0]),&f.store,&stage.id,&original,&cancelled).is_err());
}
