//! Strict Root-owned immutable file/CAS fixture for the actual SDK capture
//! helper. This is DATA reader qualification, never SDK/source/mount admission.
use super::*;
use rudder_engine::{content::ContentStore,
    directory::{DirectoryEntry,EntryNode,PagedDirectory},
    protocol::{canonical_json,ObjectKind,ObjectRef},
    store::{LocalImageImport,baked_image::{BakedImageInventoryData,BakedImageRoots,BAKED_IMAGE_LAYOUT}}};
use std::os::unix::fs::PermissionsExt;

struct Fixture {path:PathBuf,inventory:ObjectRef,payload:ObjectRef,descriptors:Vec<OwnedFd>,fence:Arc<dyn ImageImportFence>}
impl Fixture {
    fn new()->Self {
        // Do not emulate Root with chown, a UID fixture shim or relaxed capture.
        assert_eq!(unsafe{libc::geteuid()},0,"actual Root required");
        assert_eq!(unsafe{libc::getegid()},0,"actual Root group required");
        let parent=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};
        let path=parent.join(format!("rudder-sdk-baked-root-test-{}",uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        let store=LocalStore::open(path.join("cas")).unwrap();
        let fence:Arc<dyn ImageImportFence>=Arc::new(CaptureFence(FinalizationBudget::begin(Duration::from_secs(30)).unwrap()));
        let mut import=None;LocalImageImport::stage_retaining(&mut import,store.clone(),fence.clone()).unwrap();let import=import.unwrap();
        let payload=ContentStore::put_blob(&import,b"actual SDK baked descriptor fixture\n").unwrap();
        let installer=PagedDirectory::new(&import).build(0o555,[DirectoryEntry::new(b"compiler",EntryNode::File{
            object:payload.clone(),logical_size:payload.size_bytes,mode:0o555}).unwrap()]).unwrap();
        let manifest_bytes=b"immutable SDK descriptor manifest";
        let manifest=ObjectRef::from_bytes(ObjectKind::Blob,manifest_bytes);import.receive_manifest(&manifest,manifest_bytes).unwrap();
        let ack=import.commit_pins("fixture-sdk",&installer,"fixture-manifest",&manifest).unwrap();
        let objects=import.objects_after_commit(&ack).unwrap();let retained=store.retention().unwrap();
        let retention=fs::read(path.join("cas/retention.json")).unwrap();
        let roots=BakedImageRoots{format_version:1,manifest,installer_input:installer};
        let inventory=BakedImageInventoryData{format_version:1,layout:BAKED_IMAGE_LAYOUT.into(),roots:roots.clone(),
            total_bytes:objects.iter().map(|r|r.size_bytes).sum(),objects,
            retention:ObjectRef::from_bytes(ObjectKind::Blob,&retention),retention_generation:retained.generation,pins:retained.pins};
        inventory.validate().unwrap();let raw=canonical_json(&inventory).unwrap();let inventory=ObjectRef::from_bytes(ObjectKind::Blob,&raw);
        fs::write(path.join("inventory.json"),raw).unwrap();fs::write(path.join("pin.json"),canonical_json(&roots).unwrap()).unwrap();
        fs::write(path.join("manifest.json"),manifest_bytes).unwrap();fs::write(path.join("compiler-source.json"),b"fixture source-binding DATA").unwrap();
        for name in ["inventory.json","pin.json","manifest.json","compiler-source.json","cas/retention.json","cas/lock"] {
            fs::set_permissions(path.join(name),fs::Permissions::from_mode(0o444)).unwrap();
        }
        for shard in fs::read_dir(path.join("cas/objects")).unwrap(){fs::set_permissions(shard.unwrap().path(),fs::Permissions::from_mode(0o555)).unwrap();}
        for name in ["cas/objects","cas/tmp","cas",""]{fs::set_permissions(path.join(name),fs::Permissions::from_mode(0o555)).unwrap();}
        // Exactly the production descriptor layout; no inner inventory copy.
        let open_dir=|p:&Path|->OwnedFd{OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(p).unwrap().into()};
        let descriptors=vec![open_dir(&path),open_dir(&path.join("cas")),OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path.join("inventory.json")).unwrap().into()];
        Self{path,inventory,payload,descriptors,fence}
    }
    fn identities(&self)->Vec<(u64,u64)>{self.descriptors.iter().map(|fd|{let m=File::from(fd.try_clone().unwrap()).metadata().unwrap();(m.dev(),m.ino())}).collect()}
}
impl Drop for Fixture {fn drop(&mut self){
    // Only this test-owned fixture is made writable for deletion.
    fn writable(path:&Path){let Ok(m)=fs::symlink_metadata(path)else{return};if !m.is_dir()||m.file_type().is_symlink(){return}
        fs::set_permissions(path,fs::Permissions::from_mode(0o700)).unwrap();for child in fs::read_dir(path).unwrap(){writable(&child.unwrap().path());}}
    writable(&self.path);fs::remove_dir_all(&self.path).unwrap();
}}
fn enoent(error:&anyhow::Error)->bool{error.chain().any(|cause|cause.downcast_ref::<std::io::Error>().is_some_and(|e|e.raw_os_error()==Some(libc::ENOENT)))}

#[test]
#[ignore="requires actual UID0/GID0 immutable SDK fixture; never relax reader ownership"]
fn sdk_baked_capture_uses_original_parent_and_retains_all_three_descriptors(){
    let f=Fixture::new();let identities=f.identities();assert_ne!(identities[0],identities[1]);
    assert!(!f.path.join("cas/inventory.json").exists());assert!(!f.path.join("cas/cas").exists());
    let mut wrong=None;
    let error=BakedImageCas::capture_retaining(&mut wrong,f.descriptors[1].try_clone().unwrap(),f.descriptors[2].try_clone().unwrap(),f.inventory.clone(),f.fence.clone()).unwrap_err();
    assert!(enoent(&error),"original wrong root must reproduce ENOENT: {error:#}");
    assert!(wrong.is_some());assert!(wrong.as_ref().unwrap().inventory_data().is_err());
    let mut baked=None;
    capture_sdk_baked_retaining(&mut baked,&f.descriptors,f.inventory.clone(),f.fence.clone()).unwrap();
    assert_eq!(baked.as_ref().unwrap().inventory_data().unwrap().roots.installer_input.kind,ObjectKind::Directory);
    let mut read=None;baked.as_ref().unwrap().read_object_retaining(&mut read,f.payload.clone(),f.fence.clone()).unwrap();
    assert_eq!(read.as_ref().unwrap().verified_bytes().unwrap(),b"actual SDK baked descriptor fixture\n");
    assert_eq!(f.identities(),identities);
    // A held inventory FD cannot authorize a missing or replaced named file.
    fs::set_permissions(&f.path,fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(f.path.join("inventory.json"),f.path.join("original-inventory.json")).unwrap();
    fs::set_permissions(&f.path,fs::Permissions::from_mode(0o555)).unwrap();
    let mut missing=None;let error=capture_sdk_baked_retaining(&mut missing,&f.descriptors,f.inventory.clone(),f.fence.clone()).unwrap_err();
    assert!(enoent(&error));assert!(missing.is_some());assert!(missing.as_ref().unwrap().inventory_data().is_err());
    fs::set_permissions(&f.path,fs::Permissions::from_mode(0o755)).unwrap();
    fs::copy(f.path.join("original-inventory.json"),f.path.join("inventory.json")).unwrap();
    fs::set_permissions(f.path.join("inventory.json"),fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&f.path,fs::Permissions::from_mode(0o555)).unwrap();
    let mut replaced=None;assert!(capture_sdk_baked_retaining(&mut replaced,&f.descriptors,f.inventory.clone(),f.fence.clone()).is_err());
    assert!(replaced.is_some());assert!(replaced.as_ref().unwrap().inventory_data().is_err());
    assert_eq!(f.identities(),identities);
    println!("\nRUDDER_SDK_BAKED_PARENT_DESCRIPTOR_V1=passed");
}
