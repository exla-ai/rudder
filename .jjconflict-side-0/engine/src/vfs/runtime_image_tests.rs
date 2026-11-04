//! Actual local DATA only. No Root grant, mount, helper, model or provider.
use super::*;
use crate::{content::{ContentStore,FileNode,FILE_LAYOUT},directory::{DirectoryEntry,EntryNode},
    store::{LocalImageImport,baked_image::{BakedImageInventoryData,BakedImageRoots,BAKED_IMAGE_LAYOUT}}};
use std::{os::fd::OwnedFd,sync::Weak,time::Instant};
struct Budget{end:Instant,cancel:AtomicBool}
impl Budget{fn new()->Arc<Self>{Arc::new(Self{end:Instant::now()+Duration::from_secs(30),cancel:AtomicBool::new(false)})}}
impl ImageImportFence for Budget{fn remaining(&self)->Result<Duration>{ensure!(!self.cancel.load(Ordering::Acquire),"fixture cancelled");
    let r=self.end.saturating_duration_since(Instant::now());ensure!(!r.is_zero(),"fixture expired");Ok(r)}}
struct Admission{calls:AtomicUsize,closed:AtomicBool}
impl Admission{fn new()->Arc<Self>{Arc::new(Self{calls:AtomicUsize::new(0),closed:AtomicBool::new(false)})}}
impl RuntimeImageReadRestriction for Admission{fn check_new_read(&self,original:&dyn ImageImportFence)->Result<()>{
    original.remaining()?;self.calls.fetch_add(1,Ordering::AcqRel);ensure!(!self.closed.load(Ordering::Acquire),"fixture restriction closed");Ok(())}}
struct Fixture{_temporary:tempfile::TempDir,root:PathBuf,local:LocalStore,baked:Arc<BakedImageCas>,payload:ObjectRef,file:ObjectRef,installer:ObjectRef}
impl Fixture{
    fn new(variant:u8)->Self{
        let temporary=if Path::new("/workspace").is_dir(){tempfile::tempdir_in("/workspace").unwrap()}else{tempfile::tempdir().unwrap()};
        let root=temporary.path().join("baked");fs::create_dir(&root).unwrap();let store=LocalStore::open(root.join("cas")).unwrap();let mut importer=None;
        LocalImageImport::stage_retaining(&mut importer,store.clone(),Budget::new()).unwrap();let import=importer.unwrap();
        let payload=ContentStore::put_blob(&import,&vec![b'R';128*1024]).unwrap();
        let file=ContentStore::put_file_node(&import,&FileNode{format_version:1,layout:FILE_LAYOUT.into(),level:0,logical_size:payload.size_bytes,children:vec![payload.clone()]}).unwrap();
        let mut entries=Vec::new();for name in [b"alias".as_slice(),b"same",b"tool"]{entries.push(DirectoryEntry::new(name,EntryNode::File{object:file.clone(),logical_size:payload.size_bytes,mode:if variant==4&&name==b"alias"{0o444}else{0o555}}).unwrap());}
        if variant!=1{let paths=if variant==2{vec![vec![hex::encode(b"alias")],vec![hex::encode(b"missing")]]}else{vec![vec![hex::encode(b"alias")],vec![hex::encode(b"tool")]]};
            let topology=ContentStore::put_blob(&import,&canonical_json(&HardlinkTopology{format_version:1,groups:vec![HardlinkGroup{paths_hex:paths}]}).unwrap()).unwrap();
            entries.push(DirectoryEntry::new(b"hardlinks.json",EntryNode::File{logical_size:topology.size_bytes,object:topology,mode:0o444}).unwrap());}
        let installer=PagedDirectory::new(&import).build(0o555,entries).unwrap();let manifest=ObjectRef::from_bytes(ObjectKind::Blob,b"fixture full image DATA");
        import.receive_manifest(&manifest,b"fixture full image DATA").unwrap();let ack=import.commit_pins("image",&installer,"manifest",&manifest).unwrap();
        let mut objects=import.objects_after_commit(&ack).unwrap();if variant==3{objects.retain(|r|r!=&file);}
        let state=store.retention().unwrap();let roots=BakedImageRoots{format_version:1,manifest,installer_input:installer.clone()};
        let raw=fs::read(root.join("cas/retention.json")).unwrap();let inventory=BakedImageInventoryData{format_version:1,layout:BAKED_IMAGE_LAYOUT.into(),
            total_bytes:objects.iter().map(|r|r.size_bytes).sum(),objects,roots:roots.clone(),retention:ObjectRef::from_bytes(ObjectKind::Blob,&raw),retention_generation:state.generation,pins:state.pins};
        let raw=canonical_json(&inventory).unwrap();let expected=ObjectRef::from_bytes(ObjectKind::Blob,&raw);
        fs::write(root.join("inventory.json"),raw).unwrap();fs::write(root.join("pin.json"),canonical_json(&roots).unwrap()).unwrap();fs::write(root.join("manifest.json"),b"fixture full image DATA").unwrap();drop(import);
        for name in ["inventory.json","pin.json","manifest.json","cas/lock","cas/retention.json"]{fs::set_permissions(root.join(name),fs::Permissions::from_mode(0o444)).unwrap();}
        for entry in fs::read_dir(root.join("cas/objects")).unwrap(){fs::set_permissions(entry.unwrap().path(),fs::Permissions::from_mode(0o555)).unwrap();}
        for name in ["cas/objects","cas/tmp","cas",""]{fs::set_permissions(root.join(name),fs::Permissions::from_mode(0o555)).unwrap();}
        let root_fd:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&root).unwrap().into();
        let inventory_fd:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(root.join("inventory.json")).unwrap().into();let mut baked=None;
        BakedImageCas::capture_local_fixture_retaining(&mut baked,root_fd,inventory_fd,expected,Budget::new()).unwrap();
        let local=LocalStore::open(temporary.path().join("local")).unwrap();Self{_temporary:temporary,root,local,baked:baked.unwrap(),payload,file,installer}
    }
    fn capture(&self)->ReadonlyRuntimeImage{let mut out=None;ReadonlyRuntimeImage::capture_retaining(&mut out,self.local.clone(),self.baked.clone(),Admission::new(),Budget::new()).unwrap();out.unwrap()}
    fn path(&self,r:&ObjectRef)->PathBuf{self.root.join("cas/objects").join(&r.digest[..2]).join(&r.digest[2..])}
    fn corrupt(&self,r:&ObjectRef){let path=self.path(r);fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'!';r.size_bytes as usize]).unwrap();fs::set_permissions(path,fs::Permissions::from_mode(0o444)).unwrap();}
}
impl Drop for Fixture{fn drop(&mut self){fn writable(path:&Path){let Ok(m)=fs::symlink_metadata(path)else{return;};if !m.is_dir()||m.file_type().is_symlink(){return;}
    let _=fs::set_permissions(path,fs::Permissions::from_mode(0o700));if let Ok(entries)=fs::read_dir(path){for e in entries.flatten(){writable(&e.path());}}}writable(&self.root);}}

#[test]
fn actual_adapter_reads_paged_filenodes_and_genuine_hardlink_identity_without_local_copy(){
    let f=Fixture::new(0);let image=f.capture();assert_eq!(image.installer_root().unwrap(),&f.installer);let view=image.view().unwrap();
    assert!(view.lookup(1,b"tool").is_err()); // No unscoped content/DB bypass.
    linux::test_image_callback(&view,|v|{let tool=v.lookup(1,b"tool")?;let alias=v.lookup(1,b"alias")?;let same=v.lookup(1,b"same")?;
        assert_eq!(tool.ino,alias.ino);assert_eq!(tool.nlink,2);assert_ne!(tool.ino,same.ino);assert_eq!(same.nlink,1);
        assert_eq!(v.read(tool.ino,4096,31)?,vec![b'R';31]);assert_eq!(v.entries(1)?.len(),4);
        assert!(v.inherited_file(&f.payload).is_err());Ok(())}).unwrap();
    assert!(!f.local.object_path(&f.payload).unwrap().exists());assert!(!f.local.object_path(&f.file).unwrap().exists());
    assert!(f.local.retention().unwrap().pins.is_empty());assert_eq!(image.retained_requests_data().unwrap(),0);
}
#[test]
fn readonly_image_writes_forks_and_checkpoints_refuse_even_without_kernel_ro_mount(){
    let f=Fixture::new(0);let image=f.capture();let view=image.view().unwrap();
    linux::test_image_callback(&view,|v|{let file=v.lookup(1,b"tool")?;
        for error in [v.write(file.ino,0,b"bad",true).unwrap_err(),v.create(1,b"bad",NodeKind::File,0o600,b"").unwrap_err(),
            v.set_attr(file.ino,Some(0o600),None,None,None).unwrap_err(),v.unlink(1,b"tool",false).unwrap_err(),
            v.link(file.ino,1,b"bad").unwrap_err(),v.rename(1,b"tool",1,b"bad",0).unwrap_err(),v.seal().unwrap_err(),v.checkpoint_snapshot_unmounted().unwrap_err(),
            v.open_handle(file.ino,libc::O_RDWR,false).unwrap_err()]{assert_eq!(errno(&error),libc::EROFS);}
        assert_eq!(v.read(file.ino,0,7)?,vec![b'R';7]);Ok(())}).unwrap();
    assert_eq!(errno(&view.manager().prepare_paged(&f.installer).unwrap_err()),libc::EROFS);
    assert!(view.manager().fork_paged(&Uuid::new_v4().to_string(),&f.installer,OwnerEpoch{owner:"data".into(),epoch:1}).is_err());
    assert!(f.local.retention().unwrap().pins.is_empty());
}
#[test]
fn constructor_requires_explicit_valid_topology_and_complete_declared_membership(){
    for variant in [1,2,3,4]{let f=Fixture::new(variant);let mut out=None;
        assert!(ReadonlyRuntimeImage::capture_retaining(&mut out,f.local.clone(),f.baked.clone(),Admission::new(),Budget::new()).is_err());
        let owner=out.unwrap();assert!(!owner.ready && owner.view().is_err());assert!(owner.composed.is_some());assert!(f.local.retention().unwrap().pins.is_empty());}
}
#[test]
fn corrupted_metadata_keeps_constructor_read_and_never_issues_a_floor(){
    let f=Fixture::new(0);f.corrupt(&f.file);let mut out=None;
    assert!(ReadonlyRuntimeImage::capture_retaining(&mut out,f.local.clone(),f.baked.clone(),Admission::new(),Budget::new()).is_err());
    let owner=out.unwrap();assert!(owner.pending.is_some());assert!(owner.pending.as_ref().unwrap().verified_bytes().is_err());assert!(owner.view().is_err());
}
#[test]
fn original_constructor_cancel_is_retained_and_later_reads_do_not_renew_it(){
    let f=Fixture::new(0);let cancelled=Budget::new();cancelled.cancel.store(true,Ordering::Release);let mut failed=None;
    assert!(ReadonlyRuntimeImage::capture_retaining(&mut failed,f.local.clone(),f.baked.clone(),Admission::new(),cancelled).is_err());assert!(failed.unwrap().composed.is_none());
    let original=Budget::new();let admission=Admission::new();let mut out=None;
    ReadonlyRuntimeImage::capture_retaining(&mut out,f.local.clone(),f.baked.clone(),admission.clone(),original.clone()).unwrap();let image=out.unwrap();
    original.cancel.store(true,Ordering::Release);assert!(original.remaining().is_err());
    linux::test_image_callback(&image.view().unwrap(),|v|{let file=v.lookup(1,b"tool")?;assert_eq!(v.read(file.ino,0,2)?,b"RR");Ok(())}).unwrap();
    assert_eq!(admission.calls.load(Ordering::Acquire),1);assert!(original.remaining().is_err());
}
#[test]
fn one_actual_request_is_reused_nested_and_new_admission_close_does_not_renew_it(){
    let f=Fixture::new(0);let image=f.capture();let view=image.view().unwrap();let mut request=None;
    assert!(view.begin_readonly_image_request_retaining(&mut request).unwrap());let request=request.as_mut().unwrap();let constraint=request.context.clock.budget.constraint();
    image.close_admission();request.invoke(|v|{let mut nested=None;assert!(v.begin_readonly_image_request_retaining(&mut nested).is_err());
        let n=v.lookup(1,b"tool")?;assert_eq!(v.read(n.ino,0,3)?,b"RRR");Ok(())}).unwrap();
    assert_eq!(request.context.clock.budget.constraint(),constraint);request.finish_retaining().unwrap();assert_eq!(image.retained_requests_data().unwrap(),0);
    assert!(linux::test_image_callback(&view,|v|v.attr(1)).is_err());
}
#[test]
fn payload_failure_retains_actual_request_carrier_until_explicit_closed_transfer(){
    let f=Fixture::new(0);let image=f.capture();let view=image.view().unwrap();f.corrupt(&f.payload);
    assert!(linux::test_image_callback(&view,|v|{let n=v.lookup(1,b"tool")?;v.read(n.ino,0,16)}).is_err());
    assert_eq!(image.retained_requests_data().unwrap(),1);let mut retained=None;assert!(image.take_retained_request(&mut retained).is_err());
    image.close_admission();assert!(image.take_retained_request(&mut retained).unwrap());let mut read=None;
    retained.as_ref().unwrap().take_retained_read(&mut read).unwrap();assert_eq!(read.as_ref().unwrap().reference_data(),&f.payload);assert!(read.as_ref().unwrap().verified_bytes().is_err());
    // No worker was spawned in this DATA test. This transfer is not FUSE/TLS,
    // helper/actor drain or permission to unmount/release the actual Root owner.
}
#[test]
fn actual_view_keeps_readonly_backing_lifetime_after_constructor_owner_drop(){
    let f=Fixture::new(0);let image=f.capture();let weak:Weak<Floor>=Arc::downgrade(image.floor.as_ref().unwrap());let view=image.view().unwrap();drop(image);
    assert!(weak.upgrade().is_some());linux::test_image_callback(&view,|v|{let n=v.lookup(1,b"tool")?;assert_eq!(v.read(n.ino,0,4)?,b"RRRR");Ok(())}).unwrap();
    drop(view);assert!(weak.upgrade().is_none());
}

#[test]
fn cancelled_idle_finish_keeps_original_floor_slot_until_explicit_closed_transfer(){
    let f=Fixture::new(0);let image=f.capture();let view=image.view().unwrap();let mut request=None;
    assert!(view.begin_readonly_image_request_retaining(&mut request).unwrap());let request=request.as_mut().unwrap();
    request.invoke(|v|v.attr(1)).unwrap(); // The actual synchronous callback returned.
    assert_eq!(request.context.session.state_data(),ComposedSessionState::Idle);
    let original=request.context.clock.budget.constraint();let index=request.slot.unwrap();
    request.context.clock.budget.cancel(); // Deterministic original-clock loss, no sleeps.
    assert!(request.finish_retaining().is_err());assert_eq!(request.slot,Some(index));
    assert_eq!(request.context.clock.budget.constraint(),original);
    assert!(request.context.clock.closed.load(Ordering::Acquire));
    assert!(image.floor.as_ref().unwrap().requests.try_lock().unwrap()[index].as_ref()
        .is_some_and(|retained|Arc::ptr_eq(retained,&request.context)));
    assert_eq!(image.retained_requests_data().unwrap(),1);
    let mut recovery=None;assert!(image.take_retained_request(&mut recovery).is_err());
    image.close_admission();assert!(image.take_retained_request(&mut recovery).unwrap());
    assert!(Arc::ptr_eq(&recovery.as_ref().unwrap().context,&request.context));
    assert_eq!(recovery.as_ref().unwrap().context.clock.budget.constraint(),original);
    assert!(recovery.as_ref().unwrap().context.clock.budget.remaining().is_err());
    assert_eq!(image.retained_requests_data().unwrap(),0);
    // Slot transfer kept the SAME actual context; no FUSE/helper/TLS join or
    // Root retirement permission is asserted by this synchronous DATA test.
}
