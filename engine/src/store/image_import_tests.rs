//! Real local SQLite-free CAS/OS mechanics; synthetic restriction clock only.
//! No Root/native image/project/lease authority is fabricated by these tests.
use super::*;
use crate::{content::StagedContentStore,directory::{DirectoryEntry,EntryNode,LocalDirectoryStore,PagedDirectory}};
use std::{sync::atomic::{AtomicBool,Ordering},time::Instant};

struct Fence {end:Instant,cancelled:AtomicBool}
impl Fence {fn new(time:Duration)->Arc<Self>{Arc::new(Self{end:Instant::now()+time,cancelled:AtomicBool::new(false)})}}
impl ImageImportFence for Fence {fn remaining(&self)->Result<Duration>{
    ensure!(!self.cancelled.load(Ordering::Acquire),"fixture original cancelled");
    self.end.checked_duration_since(Instant::now()).filter(|r|!r.is_zero()).context("fixture original expired")
}}
struct Fixture {store:LocalStore,_directory:tempfile::TempDir}
impl Fixture {fn new()->Self {let d=if Path::new("/workspace").is_dir(){tempfile::tempdir_in("/workspace").unwrap()}else{tempfile::tempdir().unwrap()};let store=LocalStore::open(d.path().join("cas")).unwrap();Self{store,_directory:d}}}
fn bulk(f:&Fixture)->(LocalImageImport,Arc<Fence>){
    let fence=Fence::new(Duration::from_secs(10));let mut out=None;
    LocalImageImport::stage_retaining(&mut out,f.store.clone(),fence.clone()).unwrap();(out.unwrap(),fence)
}
fn entries(mut put:impl FnMut(&[u8])->ObjectRef)->Vec<DirectoryEntry>{
    (0..48).map(|i|{let bytes=format!("real unique fixture payload {i:03}").into_bytes();let object=put(&bytes);
        DirectoryEntry::new(format!("file-{i:03}").as_bytes(),EntryNode::File{object,logical_size:bytes.len()as u64,mode:0o444}).unwrap()}).collect()
}
fn graph(import:&LocalImageImport)->(ObjectRef,ObjectRef){
    let children=entries(|bytes|ContentStore::put_blob(import,bytes).unwrap());
    let installer=PagedDirectory::new(import).build(0o555,children).unwrap();
    let bytes=b"separate fixture manifest DATA";let manifest=ObjectRef::from_bytes(ObjectKind::Blob,bytes);
    import.receive_manifest(&manifest,bytes).unwrap();(installer,manifest)
}

#[test]
fn image_import_real_sync_count_drops_and_both_durable_roots_match_ordinary() {
    let ordinary=Fixture::new();ACTUAL_SYNCS.with(|n|n.set(0));
    let lease=ordinary.store.begin_staging(Duration::from_secs(30)).unwrap();
    let content=StagedContentStore::new(&ordinary.store,&lease.id).unwrap();
    let children=entries(|bytes|ContentStore::put_blob(&content,bytes).unwrap());
    let installer=PagedDirectory::new(&LocalDirectoryStore::new(&ordinary.store,&lease.id)).build(0o555,children).unwrap();
    let bytes=b"separate fixture manifest DATA";let manifest=ObjectRef::from_bytes(ObjectKind::Blob,bytes);
    ordinary.store.receive_staged(&lease.id,&manifest,bytes).unwrap();
    ordinary.store.pin("image",&installer).unwrap();ordinary.store.pin("manifest",&manifest).unwrap();
    let ordinary_syncs=ACTUAL_SYNCS.with(|n|n.get());
    let f=Fixture::new();ACTUAL_SYNCS.with(|n|n.set(0));let (import,_)=bulk(&f);
    let actual=graph(&import);assert_eq!(actual,(installer.clone(),manifest.clone()));
    assert_eq!(import.stats_data().unwrap().object_file_syncs,0,"staged refs are not durability ACKs");
    let ack=import.commit_pins("image",&installer,"manifest",&manifest).unwrap();
    let bulk_syncs=ACTUAL_SYNCS.with(|n|n.get());let stats=import.stats_data().unwrap();
    assert!(bulk_syncs<ordinary_syncs,"actual successful sync calls: bulk={bulk_syncs} ordinary={ordinary_syncs}");
    assert_eq!(stats.object_file_syncs,stats.unique_objects);
    import.assert_pins_current(&ack).unwrap();
    let retained=LocalStore::open(f.store.root()).unwrap().retention().unwrap();
    assert!(retained.image_import_holds.is_empty());assert!(retained.staging.is_empty());
    assert_eq!(retained.pins.get("image"),Some(&installer));assert_eq!(retained.pins.get("manifest"),Some(&manifest));
    assert_eq!(import.objects_after_commit(&ack).unwrap(),{
        let mut all=BTreeSet::new();all.extend(f.store.closure(&installer).unwrap());all.insert(manifest);all.into_iter().collect::<Vec<_>>()});
}

#[test]
fn image_import_private_pending_bytes_do_not_shadow_ordinary_durable_put() {
    let f=Fixture::new();let known=f.store.put_blob(b"existing agent content").unwrap();let (import,_)=bulk(&f);
    let bytes=b"pending shared CAS content";let reference=ObjectRef::from_bytes(ObjectKind::Blob,bytes);
    import.receive(&reference,bytes).unwrap();assert!(!f.store.object_path(&reference).unwrap().exists());
    assert_eq!(ContentStore::read_object(&import,&reference).unwrap(),bytes);
    // Existing agents can read shared CAS between image phases.
    let source=f.store.clone();let worker=std::thread::spawn(move||source.read_object(&known));
    assert_eq!(worker.join().unwrap().unwrap(),b"existing agent content");
    let source=f.store.clone();let r=reference.clone();let writer=std::thread::spawn(move||source.receive(&r,bytes));
    writer.join().unwrap().unwrap();
    let held=open_file(&f.store.object_path(&reference).unwrap(),false,false).unwrap();let original=identity(&held).unwrap();
    let installer=PagedDirectory::new(&import).build(0o555,[DirectoryEntry::new(b"file",EntryNode::File{object:reference.clone(),logical_size:bytes.len()as u64,mode:0o444}).unwrap()]).unwrap();
    let manifest=ObjectRef::from_bytes(ObjectKind::Blob,b"manifest");import.receive_manifest(&manifest,b"manifest").unwrap();
    let ack=import.commit_pins("image",&installer,"manifest",&manifest).unwrap();import.assert_pins_current(&ack).unwrap();
    let current=identity(&open_file(&f.store.object_path(&reference).unwrap(),false,false).unwrap()).unwrap();
    assert_eq!((current.dev,current.ino),(original.dev,original.ino),"conditional publication replaced ordinary original inode");
    assert_eq!(f.store.read_object(&reference).unwrap(),bytes);
}

#[test]
fn image_import_cancel_and_stage_abandon_never_release_nonexpiring_hold() {
    let f=Fixture::new();let (import,fence)=bulk(&f);let reference=ObjectRef::from_bytes(ObjectKind::Blob,b"pending");
    import.receive(&reference,b"pending").unwrap();fence.cancelled.store(true,Ordering::Release);
    let missing=ObjectRef::from_bytes(ObjectKind::Directory,b"{}");let manifest=ObjectRef::from_bytes(ObjectKind::Blob,b"manifest");
    assert!(import.commit_pins("image",&missing,"manifest",&manifest).is_err());
    let lease=import.staging_id_data().unwrap();drop(import);
    f.store.abandon_staging(&lease).unwrap();
    let retained=f.store.retention().unwrap();assert!(retained.staging.is_empty());assert_eq!(retained.image_import_holds.len(),1);
    assert!(f.store.gc(Duration::ZERO).unwrap().blocked_by_staging);
}

#[test]
fn image_import_partial_sync_failure_retains_actual_fd_and_absorbs_retry() {
    let f=Fixture::new();let (import,_)=bulk(&f);let (installer,manifest)=graph(&import);
    import.borrow().unwrap().fail_at=Some(TestPoint::AfterObjectSync);
    assert!(import.commit_pins("image",&installer,"manifest",&manifest).is_err());
    {let state=import.borrow().unwrap();assert!(state.gc.is_some()&&state.io.is_some());assert!(state.ack.is_none());assert!(!state.private.is_empty());}
    let stats=import.stats_data().unwrap();assert!(stats.object_file_syncs>0);
    assert!(import.commit_pins("image",&installer,"manifest",&manifest).is_err());assert_eq!(import.stats_data().unwrap(),stats);
    // Inspect while the same original gc fd owns exclusivity, without another
    // public flock/reentrant store call. No pin was acknowledged or installed.
    let retained=f.store.read_state().unwrap();assert!(!retained.image_import_holds.is_empty());assert!(retained.pins.is_empty());
}

#[test]
fn image_import_unknown_pin_ack_keeps_actual_carrier_and_original_roots() {
    let f=Fixture::new();let (import,_)=bulk(&f);let (installer,manifest)=graph(&import);
    import.borrow().unwrap().fail_at=Some(TestPoint::AfterPinCommit);
    assert!(import.commit_pins("image",&installer,"manifest",&manifest).is_err());
    {let state=import.borrow().unwrap();assert!(state.gc.is_some()&&state.ack.is_some());assert!(state.pin_intents.is_some());}
    let retained=f.store.read_state().unwrap();assert!(retained.image_import_holds.is_empty());
    assert_eq!(retained.pins.get("image"),Some(&installer));assert_eq!(retained.pins.get("manifest"),Some(&manifest));
    assert!(import.commit_pins("different",&installer,"different-manifest",&manifest).is_err());
}

#[test]
fn image_import_final_sha_and_typed_edges_are_not_replaced_by_sync_cache() {
    let f=Fixture::new();let (import,_)=bulk(&f);let (installer,manifest)=graph(&import);
    // Complete a real flush, then corrupt one canonical payload externally.
    // This violates normal immutable CAS ownership, but must not pass a cold
    // import by relying on a saved inode/mtime/durability record.
    {let mut state=import.borrow().unwrap();import.flush(&mut state).unwrap();}
    let target=import.borrow().unwrap().received.iter().find(|r|r.kind==ObjectKind::Blob&&**r!=manifest).unwrap().clone();
    let path=f.store.object_path(&target).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&path,vec![b'!';target.size_bytes as usize]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    assert!(import.commit_pins("image",&installer,"manifest",&manifest).is_err());assert!(f.store.read_state().unwrap().pins.is_empty());
}

#[test]
fn image_import_hash_valid_parent_with_wrong_actual_child_summary_refuses_pins() {
    let f=Fixture::new();let (import,_)=bulk(&f);let (installer,manifest)=graph(&import);
    let mut value:serde_json::Value=serde_json::from_slice(&ContentStore::read_object(&import,&installer).unwrap()).unwrap();
    let count=value["entry_count"].as_u64().unwrap();value["entry_count"]=serde_json::json!(count+1);
    let bytes=canonical_json(&value).unwrap();let wrong=ObjectRef::from_bytes(ObjectKind::Directory,&bytes);
    import.receive(&wrong,&bytes).unwrap();
    assert!(import.commit_pins("image",&wrong,"manifest",&manifest).is_err());
    assert!(f.store.read_state().unwrap().pins.is_empty());
}

#[test]
fn image_import_flush_is_bounded_and_large_manifest_exception_cannot_enter_installer() {
    let f=Fixture::new();let (import,_)=bulk(&f);
    for i in 0..33 {let bytes=vec![i as u8;1024*1024];let reference=ObjectRef::from_bytes(ObjectKind::Blob,&bytes);import.receive(&reference,&bytes).unwrap();}
    {let state=import.borrow().unwrap();assert!(state.stats.flushes>=1);assert!(state.pending.len()<=FLUSH_OBJECTS);
        assert!(state.pending_bytes<=FLUSH_BYTES);assert!(state.private.len()<=state.pending.len());assert!(state.gc.is_none());}
    let bytes=vec![0x41;CHUNK as usize+1];let manifest=ObjectRef::from_bytes(ObjectKind::Blob,&bytes);
    import.receive_manifest(&manifest,&bytes).unwrap();
    let directory=DirectoryManifest {format_version:1,mode:0o555,entries:vec![super::super::DirectoryEntry::new(b"forbidden-large-file",
        super::super::EntryNode::File{object:manifest.clone(),mode:0o444}).unwrap()]};
    let encoded=canonical_json(&directory).unwrap();let installer=ObjectRef::from_bytes(ObjectKind::Directory,&encoded);
    import.receive(&installer,&encoded).unwrap();
    assert!(import.commit_pins("image",&installer,"manifest",&manifest).is_err());
    assert!(f.store.read_state().unwrap().pins.is_empty());
}

#[test]
fn image_import_old_json_compatibility_and_clock_bounded_gc_admission() {
    #[derive(Deserialize)]#[serde(deny_unknown_fields)]
    struct OldState {format_version:u32,generation:u64,pins:BTreeMap<String,ObjectRef>,staging:BTreeMap<String,StagingLease>}
    let f=Fixture::new();let plain=serde_json::to_value(f.store.retention().unwrap()).unwrap();
    assert!(plain.get("image_import_holds").is_none());assert!(serde_json::from_value::<OldState>(plain).is_ok());
    let (import,_)=bulk(&f);let active=serde_json::to_value(f.store.retention().unwrap()).unwrap();
    assert!(serde_json::from_value::<OldState>(active).is_err());drop(import);
    let lock=open_file(&f.store.root.join("lock"),true,true).unwrap();FileExt::lock_exclusive(&lock).unwrap();
    let mut out=None;let original=Fence::new(Duration::from_millis(15));let began=Instant::now();
    assert!(LocalImageImport::stage_retaining(&mut out,f.store.clone(),original).is_err());
    assert!(began.elapsed()<Duration::from_secs(1));assert!(out.is_some());drop(lock);
}

#[test]
fn image_import_committed_custody_survives_elapsed_import_clock_without_new_admission() {
    let f=Fixture::new();let (import,fence)=bulk(&f);let (installer,manifest)=graph(&import);
    let ack=import.commit_pins("image",&installer,"manifest",&manifest).unwrap();fence.cancelled.store(true,Ordering::Release);
    import.assert_pins_current(&ack).unwrap();assert!(!import.objects_after_commit(&ack).unwrap().is_empty());
    assert!(import.receive(&manifest,b"manifest").is_err());
}

#[test]
fn image_import_process_crash_retains_real_hold_without_pins_or_gc_adoption() {
    use std::process::{Command,Stdio};
    let f=Fixture::new();let mut child=Command::new(std::env::current_exe().unwrap())
        .args(["--exact","store::image_import::tests::image_import_crash_child","--ignored","--nocapture"])
        .env_clear().env("RUDDER_IMAGE_IMPORT_CRASH_STORE",f.store.root())
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let cutoff=Instant::now()+Duration::from_secs(5);
    let status=loop {if let Some(status)=child.try_wait().unwrap(){break status;}
        if Instant::now()>=cutoff {child.kill().unwrap();let status=child.wait().unwrap();panic!("owned fixture child timed out: {status}");}
        std::thread::sleep(Duration::from_millis(2));};
    assert_eq!(status.code(),Some(71));
    let reopened=LocalStore::open(f.store.root()).unwrap();let retained=reopened.retention().unwrap();
    assert_eq!(retained.image_import_holds.len(),1);assert!(retained.pins.is_empty());
    assert!(reopened.gc(Duration::ZERO).unwrap().blocked_by_staging);
}
#[test]
#[ignore="actual process crash helper, parent owns exact original child"]
fn image_import_crash_child() {
    let Some(path)=std::env::var_os("RUDDER_IMAGE_IMPORT_CRASH_STORE")else{return;};
    let store=LocalStore::open(PathBuf::from(path)).unwrap();let mut out=None;
    LocalImageImport::stage_retaining(&mut out,store,Fence::new(Duration::from_secs(5))).unwrap();
    out.as_ref().unwrap().receive(&ObjectRef::from_bytes(ObjectKind::Blob,b"unacknowledged"),b"unacknowledged").unwrap();
    std::process::exit(71);
}
