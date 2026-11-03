//! Actual same-UID DATA fixtures, never Root/image/task/pin admission. The
//! baked test constructor is cfg(test) only; production still requires Root0.
use super::*;
use crate::{content::{self,StagedContentStore,FILE_LAYOUT},directory::{DirectoryEntry,EntryNode,LocalDirectoryStore,PagedDirectory},
    store::baked_image::{BakedImageInventoryData,BakedImageRoots,BAKED_IMAGE_LAYOUT}};
use std::{os::fd::OwnedFd,sync::Weak,thread,time::Instant};

struct Clock {end:Instant,cancelled:Arc<AtomicBool>}
impl Clock {fn new()->Arc<Self>{Self::lasting(Duration::from_secs(30))}
    fn lasting(duration:Duration)->Arc<Self>{Arc::new(Self{end:Instant::now()+duration,cancelled:Arc::new(AtomicBool::new(false))})}}
impl ImageImportFence for Clock {fn remaining(&self)->Result<Duration>{
    ensure!(!self.cancelled.load(Ordering::Acquire),"fixture original cancelled");let left=self.end.saturating_duration_since(Instant::now());
    ensure!(!left.is_zero(),"fixture original expired");Ok(left)}}
struct Fixture {local:LocalStore,baked:Arc<BakedImageCas>,root:PathBuf,payload:ObjectRef,file:ObjectRef,
    installer:ObjectRef,inventory:ObjectRef,_temporary:tempfile::TempDir}
impl Fixture {
    fn new()->Self {
        let temporary=if Path::new("/workspace").is_dir(){tempfile::tempdir_in("/workspace").unwrap()}else{tempfile::tempdir().unwrap()};
        let root=temporary.path().join("baked");fs::create_dir(&root).unwrap();
        let backing=LocalStore::open(root.join("cas")).unwrap();let mut import=None;
        LocalImageImport::stage_retaining(&mut import,backing.clone(),Clock::new()).unwrap();let import=import.unwrap();
        let payload=ContentStore::put_blob(&import,&vec![b'X';128*1024]).unwrap();
        let file=ContentStore::put_file_node(&import,&FileNode{format_version:1,layout:FILE_LAYOUT.into(),level:0,
            logical_size:payload.size_bytes,children:vec![payload.clone()]}).unwrap();
        let installer=PagedDirectory::new(&import).build(0o555,vec![DirectoryEntry::new(b"tool",
            EntryNode::File{object:file.clone(),logical_size:payload.size_bytes,mode:0o555}).unwrap()]).unwrap();
        let manifest_bytes=b"composed actual immutable DATA fixture";
        let manifest=ObjectRef::from_bytes(ObjectKind::Blob,manifest_bytes);
        import.receive_manifest(&manifest,manifest_bytes).unwrap();
        let ack=import.commit_pins("fixture-image",&installer,"fixture-manifest",&manifest).unwrap();
        let objects=import.objects_after_commit(&ack).unwrap();let retained=backing.retention().unwrap();
        let roots=BakedImageRoots{format_version:1,manifest,installer_input:installer.clone()};
        let raw_retention=fs::read(root.join("cas/retention.json")).unwrap();
        let data=BakedImageInventoryData{format_version:1,layout:BAKED_IMAGE_LAYOUT.into(),roots:roots.clone(),
            total_bytes:objects.iter().map(|r|r.size_bytes).sum(),objects,
            retention:ObjectRef::from_bytes(ObjectKind::Blob,&raw_retention),retention_generation:retained.generation,pins:retained.pins};
        data.validate().unwrap();let raw=canonical_json(&data).unwrap();let inventory=ObjectRef::from_bytes(ObjectKind::Blob,&raw);
        fs::write(root.join("inventory.json"),raw).unwrap();fs::write(root.join("pin.json"),canonical_json(&roots).unwrap()).unwrap();
        fs::write(root.join("manifest.json"),manifest_bytes).unwrap();drop(import);
        for name in ["inventory.json","pin.json","manifest.json","cas/lock","cas/retention.json"]{
            fs::set_permissions(root.join(name),fs::Permissions::from_mode(0o444)).unwrap();}
        for shard in fs::read_dir(root.join("cas/objects")).unwrap(){fs::set_permissions(shard.unwrap().path(),fs::Permissions::from_mode(0o555)).unwrap();}
        for name in ["cas/objects","cas/tmp","cas",""]{fs::set_permissions(root.join(name),fs::Permissions::from_mode(0o555)).unwrap();}
        let root_fd:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&root).unwrap().into();
        let inventory_fd:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(root.join("inventory.json")).unwrap().into();
        let mut baked=None;BakedImageCas::capture_local_fixture_retaining(&mut baked,root_fd,inventory_fd,inventory.clone(),Clock::new()).unwrap();
        let local=LocalStore::open(temporary.path().join("writable")).unwrap();
        Self{local,baked:baked.unwrap(),root,payload,file,installer,inventory,_temporary:temporary}
    }
    fn compose(&self)->Arc<ComposedImageStore>{let mut out=None;
        ComposedImageStore::capture_retaining(&mut out,self.local.clone(),self.baked.clone(),Clock::new()).unwrap();out.unwrap()}
    fn baked_path(&self,r:&ObjectRef)->PathBuf{self.root.join("cas/objects").join(&r.digest[..2]).join(&r.digest[2..])}
    fn corrupt_local(&self,r:&ObjectRef){let path=self.local.object_path(r).unwrap();
        fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'Y';r.size_bytes as usize]).unwrap();
        fs::set_permissions(path,fs::Permissions::from_mode(0o444)).unwrap();}
}
impl Drop for Fixture {fn drop(&mut self){
    // Only the disposable fixture changes its own directory permissions for
    // TempDir cleanup; neither production reader performs Drop cleanup.
    fn writable(path:&Path){let Ok(m)=fs::symlink_metadata(path)else{return;};if !m.is_dir()||m.file_type().is_symlink(){return;}
        let _=fs::set_permissions(path,fs::Permissions::from_mode(0o700));if let Ok(entries)=fs::read_dir(path){for e in entries.flatten(){writable(&e.path());}}}
    writable(&self.root);
}}

#[test]
fn composed_actual_paged_directory_and_file_node_reads_fall_back_without_copying(){
    let f=Fixture::new();let source=f.compose();let session=source.read_session(Clock::new());
    let entry=PagedDirectory::new(&session).lookup(&f.installer,b"tool").unwrap().unwrap();
    assert!(matches!(entry.node,EntryNode::File{object,..} if object==f.file));
    assert_eq!(content::read_file_range(&session,&f.file,4096,97).unwrap(),vec![b'X';97]);
    for r in [&f.installer,&f.file,&f.payload]{assert!(!f.local.object_path(r).unwrap().exists());}
    let mut read=None;let clock=Clock::new();source.read_object_retaining(&mut read,f.payload.clone(),clock.clone()).unwrap();
    let read=read.unwrap();assert!(Arc::ptr_eq(&read.original,&(clock as Arc<dyn ImageImportFence>)));
    assert_eq!(read.source_data(),Some(ComposedReadSource::BakedImage));assert!(read.baked.is_some()||read.cached.is_some());assert!(!read.gc_locked);
    assert_eq!(read.verified_bytes().unwrap(),vec![b'X';128*1024]);
}

#[test]
fn composed_existing_local_identity_wins_and_no_backing_inode_is_shared(){
    let f=Fixture::new();let source=f.compose();
    // Ordinary explicit writable-store IO is separate from a read fence. It
    // may add shards/files without invalidating the captured writable root.
    f.local.receive(&f.payload,&vec![b'X';128*1024]).unwrap();
    let local=fs::metadata(f.local.object_path(&f.payload).unwrap()).unwrap();let baked=fs::metadata(f.baked_path(&f.payload)).unwrap();
    assert_ne!((local.dev(),local.ino()),(baked.dev(),baked.ino()));
    let mut read=None;source.read_object_retaining(&mut read,f.payload.clone(),Clock::new()).unwrap();
    assert_eq!(read.as_ref().unwrap().source_data(),Some(ComposedReadSource::WritableLocal));
    assert!(read.as_ref().unwrap().baked.is_none() && read.as_ref().unwrap().file.is_some());
    assert_eq!(read.as_ref().unwrap().verified_bytes().unwrap(),vec![b'X';128*1024]);
}

#[test]
fn composed_private_staging_writes_never_mutate_backing_or_another_branch(){
    let f=Fixture::new();let source=f.compose();let before=fs::read(f.root.join("inventory.json")).unwrap();
    let original=PagedDirectory::new(&source.read_session(Clock::new())).lookup(&f.installer,b"tool").unwrap().unwrap();
    let stage=f.local.begin_staging(Duration::from_secs(30)).unwrap();
    let writer=StagedContentStore::new(&f.local,&stage.id).unwrap();let directories=LocalDirectoryStore::new(&f.local,&stage.id);
    let mut roots=Vec::new();let mut private=Vec::new();
    for bytes in [b"private-A".as_slice(),b"private-B".as_slice()]{
        let object=ContentStore::put_blob(&writer,bytes).unwrap();
        let root=PagedDirectory::new(&directories).build(0o755,vec![original.clone(),DirectoryEntry::new(b"private",
            EntryNode::File{object:object.clone(),logical_size:bytes.len()as u64,mode:0o644}).unwrap()]).unwrap();
        private.push(object);roots.push(root);
    }
    assert_ne!(roots[0],roots[1]);assert_ne!(private[0],private[1]);
    let session=source.read_session(Clock::new());
    for(index,root)in roots.iter().enumerate(){let entry=PagedDirectory::new(&session).lookup(root,b"private").unwrap().unwrap();
        assert!(matches!(entry.node,EntryNode::File{object,..} if object==private[index]));
        assert_eq!(content::read_file_range(&session,&f.file,0,16).unwrap(),vec![b'X';16]);}
    assert!(PagedDirectory::new(&session).lookup(&f.installer,b"private").unwrap().is_none());
    assert_eq!(fs::read(f.root.join("inventory.json")).unwrap(),before);
    assert_eq!(ObjectRef::from_bytes(ObjectKind::Blob,&before),f.inventory);
    assert!(ContentStore::put_blob(&session,b"forbidden write").is_err());
    assert!(DirectoryStore::put(&session,ObjectKind::Blob,b"forbidden write").is_err());
    // No publish is fabricated: these private staged descriptors still refer
    // to genuine backing objects absent from writable LocalStore retention.
    assert!(!f.local.object_path(&f.payload).unwrap().exists());
}

#[test]
fn composed_present_corruption_is_never_masked_by_valid_baked_bytes(){
    let f=Fixture::new();f.local.receive(&f.payload,&vec![b'X';128*1024]).unwrap();let source=f.compose();f.corrupt_local(&f.payload);
    let mut out=None;assert!(source.read_object_retaining(&mut out,f.payload.clone(),Clock::new()).is_err());
    let read=out.as_ref().unwrap();assert!(read.file.is_some() && read.gc_locked);assert!(read.baked.is_none());assert!(read.verified_bytes().is_err());
    let mut backing=None;f.baked.read_object_retaining(&mut backing,f.payload.clone(),Clock::new()).unwrap();
    assert_eq!(backing.as_ref().unwrap().verified_bytes().unwrap(),vec![b'X';128*1024]);
    assert!(out.as_mut().unwrap().run().is_err());
}

#[test]
fn composed_typed_schema_and_ordinary_chunk_bounds_precede_any_returned_bytes(){
    let f=Fixture::new();let raw=f.local.put_blob(b"not metadata").unwrap();let source=f.compose();
    let mut typed=raw.clone();typed.kind=ObjectKind::File;let mut out=None;
    assert!(source.read_object_retaining(&mut out,typed,Clock::new()).is_err());
    assert!(out.as_ref().unwrap().file.is_some() && out.as_ref().unwrap().baked.is_none());assert!(out.as_ref().unwrap().verified_bytes().is_err());drop(out);
    for variant in [0,1,2]{let mut reference=f.payload.clone();match variant{0=>reference.size_bytes=CHUNK+1,
        1=>reference.kind=ObjectKind::EnvironmentRoot,_=>reference.digest="f".repeat(64)};
        let mut failed=None;assert!(source.read_object_retaining(&mut failed,reference,Clock::new()).is_err());
        let read=failed.as_ref().unwrap();assert!(read.file.is_none());assert!(read.verified_bytes().is_err());
    }
}

#[test]
fn composed_symlink_or_permission_errors_do_not_become_a_backing_miss(){
    for symlink in [false,true]{let f=Fixture::new();f.local.receive(&f.payload,&vec![b'X';128*1024]).unwrap();let source=f.compose();
        let path=f.local.object_path(&f.payload).unwrap();if symlink{fs::remove_file(&path).unwrap();std::os::unix::fs::symlink(f.baked_path(&f.payload),&path).unwrap();}
        else{fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();}
        let mut out=None;assert!(source.read_object_retaining(&mut out,f.payload.clone(),Clock::new()).is_err());
        assert!(out.as_ref().unwrap().baked.is_none());assert!(out.as_ref().unwrap().verified_bytes().is_err());
    }
}

#[test]
fn composed_actual_gc_wait_has_original_expiry_and_no_hidden_blocking_read(){
    let f=Fixture::new();let source=f.compose();let lock=OpenOptions::new().read(true).write(true).open(f.local.root().join("lock")).unwrap();
    FileExt::lock_exclusive(&lock).unwrap();let clock=Clock::lasting(Duration::from_millis(30));let mut out=None;let start=Instant::now();
    assert!(source.read_object_retaining(&mut out,f.payload.clone(),clock.clone()).is_err());
    assert!(start.elapsed()<Duration::from_secs(2));let read=out.as_ref().unwrap();
    assert!(Arc::ptr_eq(&read.original,&(clock.clone()as Arc<dyn ImageImportFence>)));
    assert!(read.gc.is_some() && !read.gc_locked && read.file.is_none() && read.baked.is_none());
    assert!(read.verified_bytes().is_err() && clock.remaining().is_err());drop(lock);
}

#[test]
fn composed_actual_gc_wait_can_settle_under_the_same_caller_fence(){
    let f=Fixture::new();let source=f.compose();let lock=OpenOptions::new().read(true).write(true).open(f.local.root().join("lock")).unwrap();
    FileExt::lock_exclusive(&lock).unwrap();let release=thread::spawn(move||{thread::sleep(Duration::from_millis(20));drop(lock);});
    let clock=Clock::new();let mut out=None;source.read_object_retaining(&mut out,f.payload.clone(),clock.clone()).unwrap();release.join().unwrap();
    assert!(Arc::ptr_eq(&out.as_ref().unwrap().original,&(clock as Arc<dyn ImageImportFence>)));
    assert_eq!(out.as_ref().unwrap().source_data(),Some(ComposedReadSource::BakedImage));
}

#[test]
fn composed_mid_read_cancellation_retains_real_fd_partial_bytes_and_original_clock(){
    let f=Fixture::new();f.local.receive(&f.payload,&vec![b'X';128*1024]).unwrap();let source=f.compose();let clock=Clock::new();
    let mut read=ComposedRead::stage(source.clone(),f.payload.clone(),clock.clone());read.cancel_after_first_chunk=Some(clock.cancelled.clone());
    assert!(read.run().is_err());assert!(read.file.is_some() && read.gc_locked);assert_eq!(read.bytes.len(),64*1024);
    assert!(read.baked.is_none() && read.verified_bytes().is_err());assert!(clock.remaining().is_err());
    assert_eq!(source.active.load(Ordering::Acquire),1);assert!(read.run().is_err());
    assert!(!source.fenced.load(Ordering::Acquire));
    // Another original operation remains usable; this is not a renewal or
    // retry of the cancelled owner, which stays retained and unverified.
    let mut other=None;source.read_object_retaining(&mut other,f.payload.clone(),Clock::new()).unwrap();
    assert_eq!(other.as_ref().unwrap().verified_bytes().unwrap(),vec![b'X';128*1024]);
}

#[test]
fn composed_capture_cancellation_stages_actual_owners_before_any_io(){
    let f=Fixture::new();let clock=Clock::new();clock.cancelled.store(true,Ordering::Release);let mut out=None;
    assert!(ComposedImageStore::capture_retaining(&mut out,f.local.clone(),f.baked.clone(),clock.clone()).is_err());
    let source=out.as_ref().unwrap();assert!(source.root.is_none() && source.objects.is_none() && source.lock.is_none());
    assert!(Arc::ptr_eq(&source.baked,&f.baked));assert!(Arc::ptr_eq(&source.capture_original,&(clock as Arc<dyn ImageImportFence>)));
    assert!(source.fenced.load(Ordering::Acquire));
}

#[test]
fn composed_request_restriction_failure_is_separate_from_physical_source_fencing(){
    // Restriction only, no Root/lease grant: one known no-effect refusal must
    // not be reclassified as physical source loss by a second clock sample.
    struct RefuseOnce {once:AtomicBool,original:Arc<Clock>}
    impl ImageImportFence for RefuseOnce {fn remaining(&self)->Result<Duration>{
        ensure!(!self.once.swap(false,Ordering::AcqRel),"fixture known restriction busy");self.original.remaining()}}
    let f=Fixture::new();let source=f.compose();let restriction=RefuseOnce{once:AtomicBool::new(true),original:Clock::new()};
    assert!(source.current(&restriction).is_err());assert!(!source.fenced.load(Ordering::Acquire));
    source.current(&restriction).unwrap(); // SAME original clock, not a renewed source owner.
}

#[test]
fn composed_failed_backing_sha_keeps_the_genuine_baked_read_and_no_bytes(){
    let f=Fixture::new();let source=f.compose();let path=f.baked_path(&f.payload);
    fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'Y';128*1024]).unwrap();
    fs::set_permissions(path,fs::Permissions::from_mode(0o444)).unwrap();
    let mut out=None;assert!(source.read_object_retaining(&mut out,f.payload.clone(),Clock::new()).is_err());
    let read=out.as_ref().unwrap();assert_eq!(read.source_data(),Some(ComposedReadSource::BakedImage));
    assert!(read.baked.is_some() && read.file.is_none() && read.gc.is_none());assert!(read.verified_bytes().is_err());
}

#[test]
fn composed_directory_and_gc_replacement_fence_original_custody_absorbingly(){
    for replace_gc in [false,true]{let f=Fixture::new();let source=f.compose();
        if replace_gc{let path=f.local.root().join("lock");fs::rename(&path,f.local.root().join("old-lock")).unwrap();
            fs::write(&path,b"").unwrap();fs::set_permissions(path,fs::Permissions::from_mode(0o600)).unwrap();}
        else{fs::rename(f.local.root(),f._temporary.path().join("old-writable")).unwrap();drop(LocalStore::open(f.local.root()).unwrap());}
        let mut out=None;assert!(source.read_object_retaining(&mut out,f.payload.clone(),Clock::new()).is_err());
        assert!(out.as_ref().unwrap().gc.is_none() && out.as_ref().unwrap().baked.is_none());
        assert!(source.fenced.load(Ordering::Acquire));
        if replace_gc{let path=f.local.root().join("lock");fs::remove_file(&path).unwrap();fs::rename(f.local.root().join("old-lock"),path).unwrap();}
        else{fs::remove_dir_all(f.local.root()).unwrap();fs::rename(f._temporary.path().join("old-writable"),f.local.root()).unwrap();}
        let mut restored=None;assert!(source.read_object_retaining(&mut restored,f.payload.clone(),Clock::new()).is_err());
        assert!(restored.as_ref().unwrap().verified_bytes().is_err());
    }
}

#[test]
fn composed_trait_error_keeps_the_actual_slot_until_explicit_nonblocking_transfer(){
    let f=Fixture::new();f.local.receive(&f.payload,&vec![b'X';128*1024]).unwrap();let source=f.compose();f.corrupt_local(&f.payload);
    let session=source.read_session(Clock::new());assert!(ContentStore::read_object(&session,&f.payload).is_err());
    assert_eq!(source.active.load(Ordering::Acquire),1);
    assert!(DirectoryStore::read(&session,&f.installer).is_err());let mut recovery=None;
    session.take_retained_read(&mut recovery).unwrap();assert!(recovery.as_ref().unwrap().file.is_some());
    assert!(recovery.as_ref().unwrap().gc_locked && recovery.as_ref().unwrap().verified_bytes().is_err());
    assert!(session.take_retained_read(&mut recovery).is_err());assert!(DirectoryStore::read(&session,&f.installer).is_err());
    drop(recovery);assert_eq!(source.active.load(Ordering::Acquire),0);
}

#[test]
fn composed_poisoned_expired_session_still_transfers_its_actual_failed_reader(){
    use std::panic::{catch_unwind,AssertUnwindSafe};
    let f=Fixture::new();f.local.receive(&f.payload,&vec![b'X';128*1024]).unwrap();let source=f.compose();f.corrupt_local(&f.payload);
    let clock=Clock::new();let session=source.read_session(clock.clone());
    assert!(ContentStore::read_object(&session,&f.payload).is_err());
    assert!(catch_unwind(AssertUnwindSafe(||{let _original_slot=session.state.lock().unwrap();panic!("fixture carrier boundary panic");})).is_err());
    clock.cancelled.store(true,Ordering::Release);let mut recovery=None;
    session.take_retained_read(&mut recovery).unwrap();let original=recovery.as_ref().unwrap();
    assert!(original.file.is_some() && original.gc_locked && original.baked.is_none());
    assert!(original.verified_bytes().is_err());assert!(Arc::ptr_eq(&original.original,&(clock as Arc<dyn ImageImportFence>)));
    assert_eq!(source.active.load(Ordering::Acquire),1);
}

#[test]
fn composed_read_and_capacity_retain_actual_store_owners_after_outer_drop(){
    let f=Fixture::new();let source=f.compose();let weak:Weak<ComposedImageStore>=Arc::downgrade(&source);let clock=Clock::new();
    let mut reads=Vec::new();for _ in 0..MAX_READS{let mut read=None;source.read_object_retaining(&mut read,f.payload.clone(),clock.clone()).unwrap();reads.push(read.unwrap());}
    let mut full=None;assert!(source.read_object_retaining(&mut full,f.payload.clone(),clock.clone()).is_err());
    assert!(full.as_ref().unwrap().gc.is_none() && full.as_ref().unwrap().baked.is_none());drop(full);
    assert_eq!(source.active.load(Ordering::Acquire),MAX_READS);drop(reads.pop());
    let mut next=None;source.read_object_retaining(&mut next,f.payload.clone(),clock).unwrap();drop(source);
    assert!(weak.upgrade().is_some());assert_eq!(next.as_ref().unwrap().verified_bytes().unwrap(),vec![b'X';128*1024]);
    drop(reads);drop(next);assert!(weak.upgrade().is_none());
}

#[test]
fn composed_session_state_is_immediate_observation_and_never_discards_a_reader(){
    use std::panic::{catch_unwind,AssertUnwindSafe};
    let f=Fixture::new();f.local.receive(&f.payload,&vec![b'X';128*1024]).unwrap();let source=f.compose();
    let session=source.read_session(Clock::new());assert_eq!(session.state_data(),ComposedSessionState::Idle);
    {let _held=session.state.lock().unwrap();assert_eq!(session.state_data(),ComposedSessionState::Borrowed);}
    f.corrupt_local(&f.payload);assert!(ContentStore::read_object(&session,&f.payload).is_err());
    assert_eq!(session.state_data(),ComposedSessionState::Retained);
    assert!(catch_unwind(AssertUnwindSafe(||{let _held=session.state.lock().unwrap();panic!("fixture poisoned state");})).is_err());
    assert_eq!(session.state_data(),ComposedSessionState::Poisoned);let mut retained=None;
    session.take_retained_read(&mut retained).unwrap();assert!(retained.as_ref().unwrap().file.is_some());
    assert_eq!(session.state_data(),ComposedSessionState::Poisoned);
}
