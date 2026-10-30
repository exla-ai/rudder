use rudder_engine::{protocol::{ObjectKind,ObjectRef,FORMAT_VERSION},store::{DirectoryEntry,DirectoryManifest,EntryNode,LocalStore,ReferencedMetadata}};
use std::time::Duration;

fn retained_tree(store:&LocalStore)->(ObjectRef,ObjectRef) {
    let lease=store.begin_staging(Duration::from_secs(60)).unwrap();
    let blob=store.put_blob(b"retained compiler bytes").unwrap();
    let root=store.put_directory(DirectoryManifest{format_version:FORMAT_VERSION,mode:0o755,entries:vec![DirectoryEntry::new(b"compiler",EntryNode::File{object:blob.clone(),mode:0o755}).unwrap()]}).unwrap();
    store.publish(&lease.id,"prepared",&root).unwrap();
    (root,blob)
}

#[test]
fn derived_publication_retains_transitive_aliases_and_new_outputs_across_gc() {
    let tmp=tempfile::tempdir().unwrap();let store=LocalStore::open(tmp.path()).unwrap();
    let (prepared,compiler)=retained_tree(&store);
    let lease=store.begin_staging(Duration::from_secs(60)).unwrap();
    let output=store.put_blob(b"new output").unwrap();
    let result=store.put_metadata(ObjectKind::ActionResult,&ReferencedMetadata{format_version:1,references:vec![compiler.clone(),output.clone()],payload:serde_json::json!({"trusted_driver":"fixture"})}).unwrap();
    store.publish_extending(&lease.id,"result",&result,&[prepared]).unwrap();
    store.unpin("prepared").unwrap();
    store.gc(Duration::ZERO).unwrap();
    assert_eq!(store.read_object(&compiler).unwrap(),b"retained compiler bytes");
    assert_eq!(store.read_object(&output).unwrap(),b"new output");
    assert_eq!(store.closure(&result).unwrap().len(),3);
}

#[test]
fn unknown_missing_objects_and_unpinned_or_wrongly_typed_anchors_fail_closed() {
    let tmp=tempfile::tempdir().unwrap();let store=LocalStore::open(tmp.path()).unwrap();
    let (prepared,_)=retained_tree(&store);
    let lease=store.begin_staging(Duration::from_secs(60)).unwrap();
    let missing=ObjectRef::from_bytes(ObjectKind::Blob,b"never received");
    let result=store.put_metadata(ObjectKind::ActionResult,&ReferencedMetadata{format_version:1,references:vec![missing],payload:serde_json::json!({})}).unwrap();
    assert!(store.publish_extending(&lease.id,"bad",&result,&[prepared.clone()]).is_err());
    assert!(!store.retention().unwrap().pins.contains_key("bad"));
    let mut wrong=prepared.clone();wrong.kind=ObjectKind::Blob;
    assert!(store.publish_extending(&lease.id,"bad",&result,&[wrong]).is_err());
    store.unpin("prepared").unwrap();
    assert!(store.publish_extending(&lease.id,"bad",&result,&[prepared]).is_err());
    assert!(store.retention().unwrap().staging.contains_key(&lease.id));
}

#[test]
fn gc_cannot_remove_a_derived_publication_during_retention_transfer() {
    let tmp=tempfile::tempdir().unwrap();let store=LocalStore::open(tmp.path()).unwrap();
    let (prepared,compiler)=retained_tree(&store);
    let lease=store.begin_staging(Duration::from_secs(60)).unwrap();
    let result=store.put_metadata(ObjectKind::ActionResult,&ReferencedMetadata{format_version:1,references:vec![compiler],payload:serde_json::json!({})}).unwrap();
    let other=store.clone();let collector=std::thread::spawn(move||{for _ in 0..10{other.gc(Duration::ZERO).unwrap();}});
    store.publish_extending(&lease.id,"result",&result,&[prepared]).unwrap();collector.join().unwrap();
    assert!(store.read_object(&result).is_ok());
}

#[test]
fn staged_receive_is_fenced_by_live_retention_and_never_reacquires_expired_leases() {
    let tmp=tempfile::tempdir().unwrap();let store=LocalStore::open(tmp.path()).unwrap();
    let bytes=b"transfer content";let object=ObjectRef::from_bytes(ObjectKind::Blob,bytes);
    let lease=store.begin_staging(Duration::from_secs(60)).unwrap();
    store.receive_staged(&lease.id,&object,bytes).unwrap();
    assert!(store.gc(Duration::ZERO).unwrap().blocked_by_staging);
    store.publish(&lease.id,"transferred",&object).unwrap();
    let other=ObjectRef::from_bytes(ObjectKind::Blob,b"another object");
    assert!(store.receive_staged(&lease.id,&other,b"another object").is_err());
    assert!(store.read_object(&other).is_err(),"consumed lease cannot authorize new writes");
    let expired=store.begin_staging(Duration::from_millis(1)).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    assert!(store.receive_staged(&expired.id,&other,b"another object").is_err());
    assert!(store.read_object(&other).is_err());
    assert_eq!(store.read_object(&object).unwrap(),bytes);
}

#[test]
fn retained_inventory_requires_live_root_authority_and_transfer_reads_still_check_bytes() {
    let tmp=tempfile::tempdir().unwrap();let store=LocalStore::open(tmp.path()).unwrap();
    let (root,blob)=retained_tree(&store);
    assert_eq!(store.retained_closure(&root).unwrap(),store.closure(&root).unwrap());
    store.pin_existing("source-transfer",&root).unwrap();
    store.unpin("prepared").unwrap();
    store.gc(Duration::ZERO).unwrap();
    assert!(store.retained_closure(&root).is_ok());
    // Outside mutation violates the immutable store contract. Inventory is
    // retained evidence; an actual read/transfer still detects corruption.
    let path=store.object_path(&blob).unwrap();
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o600)).unwrap(); }
    std::fs::write(path,b"corrupt outside the store").unwrap();
    assert!(store.retained_closure(&root).is_ok());
    assert!(store.read_object(&blob).is_err());
    assert!(store.closure(&root).is_err());
    store.unpin("source-transfer").unwrap();
    assert!(store.retained_closure(&root).is_err(),"unpinned bytes cannot mint retention evidence");
}
