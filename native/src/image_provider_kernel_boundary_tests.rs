//! DATA/real-filesystem mechanics only; no fabricated kernel/image authority.
use super::*;
struct Fixture {path:PathBuf,store:LocalStore}
impl Fixture {fn new()->Self {let path=std::env::temp_dir().join(format!("rudder-provider-image-test-{}",uuid::Uuid::new_v4()));fs::create_dir(&path).unwrap();let store=LocalStore::open(path.join("cas")).unwrap();Self{path,store}}}
impl Drop for Fixture {fn drop(&mut self){let _=fs::remove_dir_all(&self.path);}}
#[test]
fn provider_kernel_manifest_node_is_empty_0555_and_has_no_payload_edges() {
    let f=Fixture::new();let stage=f.store.begin_staging(Duration::from_secs(30)).unwrap();
    let first=empty_provider_directory(&f.store,&stage.id).unwrap();let expected=PagedDirectory::new(&LocalDirectoryStore::new(&f.store,&stage.id)).build(0o555,Vec::new()).unwrap();assert_eq!(first,expected);
    f.store.pin("empty-provider-fixture",&first).unwrap();
    let objects=f.store.retained_closure(&first).unwrap();assert!(objects.iter().all(|r|matches!(r.kind,ObjectKind::Directory|ObjectKind::DirectoryShard)));
}
#[test]
fn provider_kernel_exclusion_does_not_relax_actual_external_hardlink_refusal() {
    let path=std::env::temp_dir().join(format!("rudder-provider-hardlink-{}",uuid::Uuid::new_v4()));fs::create_dir(&path).unwrap();
    let captured=path.join("captured");let outside=path.join("outside-captured-root");fs::write(&captured,b"actual shared inode").unwrap();fs::hard_link(&captured,&outside).unwrap();let metadata=fs::metadata(&captured).unwrap();assert_eq!(metadata.nlink(),2);
    let one=BTreeMap::from([((metadata.dev(),metadata.ino()),(metadata.nlink(),vec![vec![hex_bytes(b"captured")]]))]);assert!(hardlink_topology(&one).is_err());
    let both=BTreeMap::from([((metadata.dev(),metadata.ino()),(metadata.nlink(),vec![vec![hex_bytes(b"captured")],vec![hex_bytes(b"outside-captured-root")]]))]);assert_eq!(hardlink_topology(&both).unwrap().groups.len(),1);fs::remove_dir_all(path).unwrap();
}
#[test]
fn provider_swap_node_contains_only_a_retained_empty_nonexecutable_blob() {
    let f=Fixture::new();let stage=f.store.begin_staging(Duration::from_secs(30)).unwrap();
    let actual=empty_provider_file(&f.store,&stage.id).unwrap();
    let expected=ObjectRef::from_bytes(ObjectKind::Blob,b"");
    let EntryNode::File{object,logical_size,mode}=actual else {panic!("expected empty file")};
    assert_eq!(object,expected);assert_eq!(logical_size,0);assert_eq!(mode,0o444);
    f.store.pin("empty-provider-tool-fixture",&object).unwrap();
    assert_eq!(f.store.retained_closure(&object).unwrap(),vec![expected]);
}
