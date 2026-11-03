//! Real immutable CAS reads through fresh FUSE-style sessions. These fixture
//! clocks and same-UID constructors create DATA, never Root/task admission.
use super::*;
use crate::{directory::{DirectoryEntry,EntryNode,PagedDirectory},
    store::baked_image::{BakedImageInventoryData,BakedImageRoots,BAKED_IMAGE_LAYOUT}};

struct Fence {cancelled:AtomicBool}
impl Fence {fn new()->Arc<Self>{Arc::new(Self{cancelled:AtomicBool::new(false)})}}
impl ImageImportFence for Fence {fn remaining(&self)->Result<Duration>{
    ensure!(!self.cancelled.load(Ordering::Acquire),"fixture original expired/cancelled");Ok(Duration::from_secs(3600))}}
struct Fixture {root:PathBuf,local:LocalStore,source:Arc<ComposedImageStore>,objects:Vec<ObjectRef>,_temporary:tempfile::TempDir}
impl Fixture {
    fn new(sizes:&[usize])->Self {
        let temporary=if Path::new("/workspace").is_dir(){tempfile::tempdir_in("/workspace").unwrap()}else{tempfile::tempdir().unwrap()};
        let root=temporary.path().join("baked");fs::create_dir(&root).unwrap();
        let backing=LocalStore::open(root.join("cas")).unwrap();let mut import=None;
        LocalImageImport::stage_retaining(&mut import,backing.clone(),Fence::new()).unwrap();let import=import.unwrap();
        let objects:Vec<_>=sizes.iter().enumerate().map(|(n,size)|ContentStore::put_blob(&import,&vec![n as u8;*size]).unwrap()).collect();
        let entries:Vec<_>=objects.iter().enumerate().map(|(n,r)|DirectoryEntry::new(format!("blob-{n:03}").as_bytes(),
            EntryNode::File{object:r.clone(),logical_size:r.size_bytes,mode:0o444}).unwrap()).collect();
        let installer=PagedDirectory::new(&import).build(0o555,entries).unwrap();
        let bytes=b"bounded cache fixture manifest";let manifest=ObjectRef::from_bytes(ObjectKind::Blob,bytes);
        import.receive_manifest(&manifest,bytes).unwrap();
        let ack=import.commit_pins("fixture-image",&installer,"fixture-manifest",&manifest).unwrap();
        let all=import.objects_after_commit(&ack).unwrap();let retained=backing.retention().unwrap();
        let roots=BakedImageRoots{format_version:1,manifest,installer_input:installer};
        let raw_retention=fs::read(root.join("cas/retention.json")).unwrap();
        let data=BakedImageInventoryData{format_version:1,layout:BAKED_IMAGE_LAYOUT.into(),roots:roots.clone(),
            total_bytes:all.iter().map(|r|r.size_bytes).sum(),objects:all,
            retention:ObjectRef::from_bytes(ObjectKind::Blob,&raw_retention),retention_generation:retained.generation,pins:retained.pins};
        let raw=canonical_json(&data).unwrap();let inventory=ObjectRef::from_bytes(ObjectKind::Blob,&raw);
        fs::write(root.join("inventory.json"),raw).unwrap();fs::write(root.join("pin.json"),canonical_json(&roots).unwrap()).unwrap();drop(import);
        for name in ["inventory.json","pin.json","cas/lock","cas/retention.json"]{fs::set_permissions(root.join(name),fs::Permissions::from_mode(0o444)).unwrap();}
        for shard in fs::read_dir(root.join("cas/objects")).unwrap(){fs::set_permissions(shard.unwrap().path(),fs::Permissions::from_mode(0o555)).unwrap();}
        for name in ["cas/objects","cas/tmp","cas",""]{fs::set_permissions(root.join(name),fs::Permissions::from_mode(0o555)).unwrap();}
        let directory=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&root).unwrap().into();
        let index=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(root.join("inventory.json")).unwrap().into();
        let mut baked=None;BakedImageCas::capture_local_fixture_retaining(&mut baked,directory,index,inventory,Fence::new()).unwrap();
        let local=LocalStore::open(temporary.path().join("local")).unwrap();let mut source=None;
        ComposedImageStore::capture_retaining(&mut source,local.clone(),baked.unwrap(),Fence::new()).unwrap();
        Self{root,local,source:source.unwrap(),objects,_temporary:temporary}
    }
    fn path(&self,n:usize)->PathBuf{let r=&self.objects[n];self.root.join("cas/objects").join(&r.digest[..2]).join(&r.digest[2..])}
    fn range(&self,n:usize,offset:u64,length:usize)->Result<Vec<u8>>{
        ContentStore::read_blob_range(&self.source.read_session(Fence::new()),&self.objects[n],offset,length)
    }
    fn reads(&self)->usize{self.source.baked_blob_reads.load(Ordering::Acquire)}
}
impl Drop for Fixture {fn drop(&mut self){
    fn writable(path:&Path){let Ok(m)=fs::symlink_metadata(path)else{return;};if !m.is_dir()||m.file_type().is_symlink(){return;}
        let _=fs::set_permissions(path,fs::Permissions::from_mode(0o700));if let Ok(entries)=fs::read_dir(path){for e in entries.flatten(){writable(&e.path());}}}
    writable(&self.root);
}}

#[test]
fn blob_cache_reuses_one_verified_four_mib_chunk_across_sixty_four_fresh_sessions(){
    let f=Fixture::new(&[CHUNK as usize]);
    for n in 0..64{assert_eq!(f.range(0,n*65536,65536).unwrap(),vec![0;65536]);}
    assert_eq!(f.reads(),1,"fresh FUSE callbacks must not re-read/hash the full chunk");
    {let cache=f.source.blob_cache.lock().unwrap();assert_eq!(cache.bytes,CHUNK as usize);assert_eq!(cache.entries.len(),1);}
    assert_eq!(f.source.active.load(Ordering::Acquire),0,"cache cannot consume an active-read slot");
    assert_eq!(f.range(0,CHUNK+1,64).unwrap(),Vec::<u8>::new());
}

#[test]
fn blob_cache_refuses_corruption_before_fill_and_after_verified_fill(){
    for warm in [false,true]{let f=Fixture::new(&[4096]);if warm{f.range(0,0,1).unwrap();}
        let path=f.path(0);fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path,vec![9;4096]).unwrap();fs::set_permissions(path,fs::Permissions::from_mode(0o444)).unwrap();
        assert!(f.range(0,0,1).is_err());assert!(f.range(0,0,1).is_err(),"physical failure remains absorbing");
    }
}

#[test]
fn blob_cache_refuses_replaced_leaf_and_wrong_content_reference(){
    let f=Fixture::new(&[4096]);f.range(0,0,1).unwrap();
    let mut wrong=f.objects[0].clone();wrong.digest="f".repeat(64);
    assert!(ContentStore::read_blob_range(&f.source.read_session(Fence::new()),&wrong,0,1).is_err());
    let path=f.path(0);let parent=path.parent().unwrap();fs::set_permissions(parent,fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&path,parent.join("old-leaf")).unwrap();fs::write(&path,vec![0;4096]).unwrap();
    fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();fs::set_permissions(parent,fs::Permissions::from_mode(0o555)).unwrap();
    assert!(f.range(0,0,1).is_err(),"same bytes at a new inode cannot adopt cached original custody");
}

#[test]
fn blob_cache_keeps_caller_fence_and_local_corruption_precedence(){
    let f=Fixture::new(&[4096]);f.range(0,0,1).unwrap();let clock=Fence::new();let session=f.source.read_session(clock.clone());
    assert_eq!(ContentStore::read_blob_range(&session,&f.objects[0],0,1).unwrap(),vec![0]);
    clock.cancelled.store(true,Ordering::Release);
    assert!(ContentStore::read_blob_range(&session,&f.objects[0],0,1).is_err());
    struct Expired(std::time::Instant);
    impl ImageImportFence for Expired{fn remaining(&self)->Result<Duration>{
        let remaining=self.0.saturating_duration_since(std::time::Instant::now());ensure!(!remaining.is_zero(),"fixture deadline expired");Ok(remaining)}}
    let expired=f.source.read_session(Arc::new(Expired(std::time::Instant::now())));
    assert!(ContentStore::read_blob_range(&expired,&f.objects[0],0,1).is_err());
    assert_eq!(f.range(0,0,1).unwrap(),vec![0],"one cancelled caller does not poison immutable DATA");
    f.local.receive(&f.objects[0],&vec![0;4096]).unwrap();let local=f.local.object_path(&f.objects[0]).unwrap();
    fs::set_permissions(&local,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&local,vec![1;4096]).unwrap();
    fs::set_permissions(&local,fs::Permissions::from_mode(0o444)).unwrap();
    assert!(f.range(0,0,1).is_err(),"a cached baked object cannot hide corrupt writable-local data");
}

#[test]
fn blob_cache_enforces_byte_bound_and_lru_with_actual_large_objects(){
    let f=Fixture::new(&vec![CHUNK as usize;17]);
    for n in 0..16{assert_eq!(f.range(n,0,1).unwrap(),vec![n as u8]);}
    f.range(0,0,1).unwrap();f.range(16,0,1).unwrap();assert_eq!(f.reads(),17);
    {let cache=f.source.blob_cache.lock().unwrap();assert_eq!(cache.bytes,MAX_BLOB_CACHE_BYTES);
        assert!(cache.entries.iter().any(|(r,_)|r==&f.objects[0]));assert!(!cache.entries.iter().any(|(r,_)|r==&f.objects[1]));}
    f.range(0,0,1).unwrap();assert_eq!(f.reads(),17);f.range(1,0,1).unwrap();assert_eq!(f.reads(),18);
}

#[test]
fn blob_cache_bounds_small_entry_count_separately_from_bytes(){
    let f=Fixture::new(&vec![1;33]);for n in 0..33{assert_eq!(f.range(n,0,1).unwrap(),vec![n as u8]);}
    let cache=f.source.blob_cache.lock().unwrap();assert_eq!(cache.entries.len(),MAX_BLOB_CACHE_ENTRIES);assert_eq!(cache.bytes,32);
    assert!(!cache.entries.iter().any(|(r,_)|r==&f.objects[0]));
}
