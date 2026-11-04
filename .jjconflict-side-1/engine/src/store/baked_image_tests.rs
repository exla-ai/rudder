//! Real file/CAS fixtures. The private same-UID capture path exercises byte and
//! FD mechanics only; it is never Root/image/namespace qualification. The public
//! constructor remains Root0/gid0-file strict, with an ignored actualRoot test.
use super::*;
use crate::{content::ContentStore, directory::{DirectoryEntry, EntryNode, PagedDirectory}};
use std::{panic::{catch_unwind, AssertUnwindSafe}, sync::{Weak, atomic::AtomicBool}, time::Instant};

struct Clock {end:Instant,cancelled:Arc<AtomicBool>}
impl Clock {fn new()->Arc<Self>{Arc::new(Self{end:Instant::now()+Duration::from_secs(30),cancelled:Arc::new(AtomicBool::new(false))})}}
impl ImageImportFence for Clock {fn remaining(&self)->Result<Duration>{
    ensure!(!self.cancelled.load(Ordering::Acquire),"fixture cancelled");let left=self.end.saturating_duration_since(Instant::now());
    ensure!(!left.is_zero(),"fixture expired");Ok(left)}}
struct Fixture {root:PathBuf,catalog:ObjectRef,payload:ObjectRef,manifest:ObjectRef,owner:(u32,u32),_temporary:tempfile::TempDir}
impl Fixture {
    fn new(manifest_bytes:Vec<u8>)->Self {
        let temporary=if Path::new("/workspace").is_dir(){tempfile::tempdir_in("/workspace").unwrap()}else{tempfile::tempdir().unwrap()};
        let root=temporary.path().join("baked");fs::create_dir(&root).unwrap();fs::set_permissions(&root,fs::Permissions::from_mode(0o700)).unwrap();
        let store=LocalStore::open(root.join("cas")).unwrap();let clock=Clock::new();let mut import=None;
        LocalImageImport::stage_retaining(&mut import,store.clone(),clock).unwrap();let import=import.unwrap();
        let payload=ContentStore::put_blob(&import,&vec![b'X';128*1024]).unwrap();
        let installer=PagedDirectory::new(&import).build(0o555,vec![DirectoryEntry::new(b"tool",EntryNode::File{object:payload.clone(),logical_size:payload.size_bytes,mode:0o555}).unwrap()]).unwrap();
        let manifest=ObjectRef::from_bytes(ObjectKind::Blob,&manifest_bytes);import.receive_manifest(&manifest,&manifest_bytes).unwrap();
        let ack=import.commit_pins("fixture-image",&installer,"fixture-manifest",&manifest).unwrap();
        let objects=import.objects_after_commit(&ack).unwrap();let retained=store.retention().unwrap();
        let raw=fs::read(root.join("cas/retention.json")).unwrap();
        let roots=BakedImageRoots{format_version:1,manifest:manifest.clone(),installer_input:installer};
        let inventory=BakedImageInventoryData{format_version:1,layout:BAKED_IMAGE_LAYOUT.into(),roots:roots.clone(),total_bytes:objects.iter().map(|r|r.size_bytes).sum(),objects,
            retention:ObjectRef::from_bytes(ObjectKind::Blob,&raw),retention_generation:retained.generation,pins:retained.pins};inventory.validate().unwrap();
        let bytes=canonical_json(&inventory).unwrap();let catalog=ObjectRef::from_bytes(ObjectKind::Blob,&bytes);
        fs::write(root.join("inventory.json"),&bytes).unwrap();fs::write(root.join("pin.json"),canonical_json(&roots).unwrap()).unwrap();fs::write(root.join("manifest.json"),manifest_bytes).unwrap();
        for name in ["inventory.json","pin.json","manifest.json","cas/retention.json","cas/lock"]{fs::set_permissions(root.join(name),fs::Permissions::from_mode(0o444)).unwrap();}
        for entry in fs::read_dir(root.join("cas/objects")).unwrap(){fs::set_permissions(entry.unwrap().path(),fs::Permissions::from_mode(0o555)).unwrap();}
        for name in ["cas/objects","cas/tmp","cas",""]{fs::set_permissions(root.join(name),fs::Permissions::from_mode(0o555)).unwrap();}
        Self{root,catalog,payload,manifest,owner:(unsafe{libc::geteuid()},unsafe{libc::getegid()}),_temporary:temporary}
    }
    fn fds(&self)->(OwnedFd,OwnedFd){
        (OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&self.root).unwrap().into(),
         OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(self.root.join("inventory.json")).unwrap().into())
    }
    fn capture(&self)->Arc<BakedImageCas>{let(root,inventory)=self.fds();let mut out=None;
        BakedImageCas::capture_local_fixture_retaining(&mut out,root,inventory,self.catalog.clone(),Clock::new()).unwrap();out.unwrap()}
    fn object_path(&self,r:&ObjectRef)->PathBuf{self.root.join("cas/objects").join(&r.digest[..2]).join(&r.digest[2..])}
    fn rewrite_file(&self,path:&Path,bytes:&[u8]){fs::set_permissions(path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(path,bytes).unwrap();fs::set_permissions(path,fs::Permissions::from_mode(0o444)).unwrap();}
}
impl Drop for Fixture {fn drop(&mut self){
    // Test-owned TempDir cleanup only. Production reader Drop performs no IO.
    fn writable(path:&Path){let Ok(meta)=fs::symlink_metadata(path)else{return;};if !meta.is_dir()||meta.file_type().is_symlink(){return;}
        let _=fs::set_permissions(path,fs::Permissions::from_mode(0o700));if let Ok(entries)=fs::read_dir(path){for child in entries.flatten(){writable(&child.path());}}}
    writable(&self.root);
}}

#[path="baked_original_versions_tests.rs"]mod original_version_tests;

#[test]
fn baked_image_real_original_pin_inventory_and_sha_before_bytes(){
    let f=Fixture::new(b"full image manifest fixture".to_vec());let source=f.capture();let mut read=None;
    source.read_object_retaining(&mut read,f.payload.clone(),Clock::new()).unwrap();
    assert_eq!(read.as_ref().unwrap().verified_bytes().unwrap(),vec![b'X';128*1024]);
    assert!(read.as_ref().unwrap().file.is_some());assert_eq!(read.as_ref().unwrap().reference_data(),&f.payload);
    let data=source.inventory_data().unwrap();assert_eq!(data.pins.len(),2);assert_eq!(data.roots.manifest,f.manifest);
    let state=source.state.lock().unwrap();let data=state.inventory.as_ref().unwrap();
    assert_eq!(state.object_versions.len(),data.objects.len());
    assert_eq!(state.shards.len(),data.objects.iter().map(|r|&r.digest[..2]).collect::<BTreeSet<_>>().len());
}

#[test]
fn baked_image_wrong_size_kind_and_nonmember_are_not_path_authority(){
    let f=Fixture::new(b"manifest".to_vec());let source=f.capture();
    for change in [0,1,2]{let mut reference=f.payload.clone();match change{0=>reference.size_bytes+=1,1=>reference.kind=ObjectKind::Directory,_=>reference.digest="f".repeat(64)};
        let mut out=None;assert!(source.read_object_retaining(&mut out,reference,Clock::new()).is_err());
        let read=out.as_ref().unwrap();assert!(read.file.is_none());assert!(read.verified_bytes().is_err());}
    let mut good=None;source.read_object_retaining(&mut good,f.payload.clone(),Clock::new()).unwrap();
}

#[test]
fn baked_image_same_size_payload_corruption_keeps_failed_fd_and_no_bytes(){
    let f=Fixture::new(b"manifest".to_vec());let source=f.capture();
    f.rewrite_file(&f.object_path(&f.payload),&vec![b'Y';128*1024]);
    let mut out=None;assert!(source.read_object_retaining(&mut out,f.payload.clone(),Clock::new()).is_err());
    assert!(out.as_ref().unwrap().file.is_some());assert!(out.as_ref().unwrap().verified_bytes().is_err());
    assert!(source.state.lock().unwrap().failed);assert_eq!(source.active.load(Ordering::Acquire),1);
    assert!(out.as_mut().unwrap().run().is_err());
}

#[test]
fn baked_image_object_and_directory_replacement_refuse_original_custody(){
    for replace_directory in [false,true]{let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let mut first=None;
        source.read_object_retaining(&mut first,f.payload.clone(),Clock::new()).unwrap();drop(first);
        let objects=f.root.join("cas/objects");fs::set_permissions(&objects,fs::Permissions::from_mode(0o755)).unwrap();
        if replace_directory {let shard=objects.join(&f.payload.digest[..2]);fs::rename(&shard,objects.join("old-shard")).unwrap();fs::create_dir(&shard).unwrap();fs::set_permissions(&shard,fs::Permissions::from_mode(0o555)).unwrap();}
        else{let shard=f.object_path(&f.payload).parent().unwrap().to_path_buf();fs::set_permissions(&shard,fs::Permissions::from_mode(0o755)).unwrap();
            fs::remove_file(f.object_path(&f.payload)).unwrap();fs::write(f.object_path(&f.payload),vec![b'X';128*1024]).unwrap();fs::set_permissions(f.object_path(&f.payload),fs::Permissions::from_mode(0o444)).unwrap();fs::set_permissions(shard,fs::Permissions::from_mode(0o555)).unwrap();}
        fs::set_permissions(&objects,fs::Permissions::from_mode(0o555)).unwrap();
        let mut read=None;assert!(source.read_object_retaining(&mut read,f.payload.clone(),Clock::new()).is_err());
        assert!(read.as_ref().unwrap().verified_bytes().is_err());assert!(source.state.lock().unwrap().failed);
    }
}

#[test]
fn baked_image_symlink_leaf_and_writable_or_hardlinked_file_refuse(){
    for variant in [0,1,2]{let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let path=f.object_path(&f.payload);
        let shard=path.parent().unwrap();fs::set_permissions(shard,fs::Permissions::from_mode(0o755)).unwrap();
        match variant {0=>{fs::remove_file(&path).unwrap();std::os::unix::fs::symlink("/etc/passwd",&path).unwrap();},
            1=>fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap(),_=>fs::hard_link(&path,shard.join("unexpected-alias")).unwrap()};
        fs::set_permissions(shard,fs::Permissions::from_mode(0o555)).unwrap();let mut out=None;
        assert!(source.read_object_retaining(&mut out,f.payload.clone(),Clock::new()).is_err());assert!(out.as_ref().unwrap().verified_bytes().is_err());}
}

#[test]
fn baked_image_explicit_manifest_bound_does_not_expand_runtime_chunks(){
    let f=Fixture::new(vec![b'M';CHUNK as usize+1]);let source=f.capture();let mut ordinary=None;
    assert!(source.read_object_retaining(&mut ordinary,f.manifest.clone(),Clock::new()).is_err());
    assert!(ordinary.as_ref().unwrap().file.is_none());
    let mut manifest=None;source.read_manifest_retaining(&mut manifest,Clock::new()).unwrap();
    assert_eq!(manifest.as_ref().unwrap().verified_bytes().unwrap().len(),CHUNK as usize+1);
    assert_eq!(manifest.as_ref().unwrap().reference_data(),&f.manifest);
}

#[test]
fn baked_image_actual_source_lifetime_and_read_reservation_survive_outer_drop(){
    let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let weak:Weak<BakedImageCas>=Arc::downgrade(&source);let mut out=None;
    source.read_object_retaining(&mut out,f.payload.clone(),Clock::new()).unwrap();drop(source);
    assert!(weak.upgrade().is_some());assert_eq!(out.as_ref().unwrap().verified_bytes().unwrap().len(),128*1024);
    assert_eq!(weak.upgrade().unwrap().active.load(Ordering::Acquire),1);drop(out);assert!(weak.upgrade().is_none());
}

#[test]
fn baked_image_capture_digest_and_partial_directory_custody_are_retained_on_error(){
    let f=Fixture::new(b"manifest".to_vec());let(root,inventory)=f.fds();let mut wrong=f.catalog.clone();wrong.digest="0".repeat(64);let mut out=None;
    assert!(BakedImageCas::capture_owned(&mut out,root,inventory,wrong,Clock::new(),f.owner).is_err());
    assert!(out.is_some());assert!(out.as_ref().unwrap().state.lock().unwrap().failed);assert!(out.as_ref().unwrap().inventory_data().is_err());
    let(root,inventory)=f.fds();fs::set_permissions(f.root.join("cas/objects"),fs::Permissions::from_mode(0o755)).unwrap();let mut dirs=None;
    assert!(BakedImageCas::capture_owned(&mut dirs,root,inventory,f.catalog.clone(),Clock::new(),f.owner).is_err());
    let state=dirs.as_ref().unwrap().state.lock().unwrap();assert!(state.cas.is_some()&&state.objects.is_some());
}

#[test]
fn baked_image_read_cancellation_and_unwind_never_expose_partial_bytes(){
    let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let clock=Clock::new();clock.cancelled.store(true,Ordering::Release);
    let mut cancelled=None;assert!(source.read_object_retaining(&mut cancelled,f.payload.clone(),clock).is_err());
    assert!(cancelled.is_some());assert!(cancelled.as_ref().unwrap().file.is_none());
    let mut original=BakedImageRead::stage(source.clone(),f.payload.clone(),false,Clock::new());original.panic_after_first_chunk=true;
    assert!(catch_unwind(AssertUnwindSafe(||original.run())).is_err());
    assert!(original.file.is_some());assert!(!original.bytes.is_empty());assert!(original.verified_bytes().is_err());
    assert!(original.source.state.is_poisoned());assert!(original.run().is_err());
}

#[test]
fn baked_image_bounded_live_read_owners_do_not_spawn_replacements(){
    let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let mut owners=Vec::new();
    for _ in 0..MAX_READS{let mut read=None;source.read_object_retaining(&mut read,f.payload.clone(),Clock::new()).unwrap();owners.push(read.unwrap());}
    let mut busy=None;assert!(source.read_object_retaining(&mut busy,f.payload.clone(),Clock::new()).is_err());assert!(busy.as_ref().unwrap().file.is_none());
    assert_eq!(source.active.load(Ordering::Acquire),MAX_READS);owners.pop();
    let mut next=None;source.read_object_retaining(&mut next,f.payload.clone(),Clock::new()).unwrap();assert_eq!(source.active.load(Ordering::Acquire),MAX_READS);
}

#[test]
fn baked_image_public_constructor_never_accepts_uid_fixture_as_root(){
    if unsafe{libc::geteuid()}==0{return;}
    let f=Fixture::new(b"manifest".to_vec());let(root,inventory)=f.fds();let mut out=None;
    assert!(BakedImageCas::capture_retaining(&mut out,root,inventory,f.catalog.clone(),Clock::new()).is_err());
    assert!(out.is_some());assert!(out.as_ref().unwrap().inventory_data().is_err());
}

#[test]
fn baked_image_writable_or_non_cloexec_input_descriptor_cannot_be_admitted(){
    for writable in [false,true]{let f=Fixture::new(b"manifest".to_vec());let(root,original_inventory)=f.fds();
        let inventory=if writable{
            let path=f.root.join("inventory.json");fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();
            let file=OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&path).unwrap();
            fs::set_permissions(path,fs::Permissions::from_mode(0o444)).unwrap();file.into()
        }else{
            let raw=unsafe{libc::fcntl(original_inventory.as_raw_fd(),libc::F_DUPFD,0)};assert!(raw>=0);
            unsafe{OwnedFd::from_raw_fd(raw)}
        };
        let mut out=None;assert!(BakedImageCas::capture_owned(&mut out,root,inventory,f.catalog.clone(),Clock::new(),f.owner).is_err());
        assert!(out.is_some());assert!(out.as_ref().unwrap().inventory_data().is_err());
    }
}

#[test]
fn baked_image_hash_valid_but_bad_typed_metadata_still_refuses_bytes(){
    let mut f=Fixture::new(b"manifest".to_vec());
    // This is deliberately only an untrusted catalog DATA member, not a new
    // complete pin or image grant. Even matching rawSHA cannot bless its schema.
    let mut data:BakedImageInventoryData=serde_json::from_slice(&fs::read(f.root.join("inventory.json")).unwrap()).unwrap();
    let bad=ObjectRef::from_bytes(ObjectKind::Directory,b"{}");let path=f.object_path(&bad);
    fs::set_permissions(f.root.join("cas/objects"),fs::Permissions::from_mode(0o755)).unwrap();
    if !path.parent().unwrap().exists(){fs::create_dir(path.parent().unwrap()).unwrap();}
    fs::set_permissions(path.parent().unwrap(),fs::Permissions::from_mode(0o755)).unwrap();fs::write(&path,b"{}").unwrap();
    fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();fs::set_permissions(path.parent().unwrap(),fs::Permissions::from_mode(0o555)).unwrap();fs::set_permissions(f.root.join("cas/objects"),fs::Permissions::from_mode(0o555)).unwrap();
    data.objects.push(bad.clone());data.objects.sort();data.total_bytes+=bad.size_bytes;let bytes=canonical_json(&data).unwrap();
    f.rewrite_file(&f.root.join("inventory.json"),&bytes);f.catalog=ObjectRef::from_bytes(ObjectKind::Blob,&bytes);
    let source=f.capture();let mut read=None;assert!(source.read_object_retaining(&mut read,bad,Clock::new()).is_err());
    assert!(read.as_ref().unwrap().file.is_some());assert!(read.as_ref().unwrap().verified_bytes().is_err());
}

#[test]
fn baked_image_partial_cancel_retains_reader_without_poisoning_other_immutable_reads(){
    let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let clock=Clock::new();
    let mut read=BakedImageRead::stage(source.clone(),f.payload.clone(),false,clock.clone());read.cancel_after_first_chunk=Some(clock.cancelled.clone());
    assert!(read.run().is_err());assert!(read.file.is_some()&&!read.bytes.is_empty());assert!(read.verified_bytes().is_err());
    assert!(!source.state.lock().unwrap().failed);assert!(read.run().is_err());
    let mut other=None;source.read_object_retaining(&mut other,f.payload.clone(),Clock::new()).unwrap();
    assert_eq!(other.as_ref().unwrap().verified_bytes().unwrap().len(),128*1024);
}

#[test]
#[ignore="requires actual Root0/gid0; byte component only, no image/task admission"]
fn baked_image_actual_root_owned_fds_and_original_pin_bytes(){
    assert_eq!((unsafe{libc::geteuid()},unsafe{libc::getegid()}),(0,0));
    let f=Fixture::new(b"manifest".to_vec());let(root,inventory)=f.fds();let mut out=None;
    BakedImageCas::capture_retaining(&mut out,root,inventory,f.catalog.clone(),Clock::new()).unwrap();
    let mut read=None;out.as_ref().unwrap().read_object_retaining(&mut read,f.payload.clone(),Clock::new()).unwrap();
    assert_eq!(read.as_ref().unwrap().verified_bytes().unwrap(),vec![b'X';128*1024]);
}
