use super::*;
use rudder_engine::{content::{self,ContentStore},directory::{EntryNode,LocalDirectoryStore,PagedDirectory},tree::HardlinkTopology};
use std::{os::unix::fs::{DirBuilderExt,PermissionsExt},time::Duration};

fn manifest_for(mut files:Vec<(String,u32,Vec<u8>)>)->(CompilerManifest,BTreeMap<String,Vec<u8>>) {
    for path in REQUIRED_FILES {if !files.iter().any(|(p,_,_)|p==path){files.push((path.into(),0o555,format!("fixture:{path}").into_bytes()));}}
    let library="prepared/tools/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-fixture.rlib";
    if !files.iter().any(|(p,_,_)|p==library){files.push((library.into(),0o444,b"small real fixture archive bytes".to_vec()));}
    let mut directories:BTreeSet<String>=REQUIRED_DIRS.iter().map(|s|s.to_string()).collect();
    for(path,_,_)in &files {let mut parent=Path::new(path).parent();while let Some(p)=parent{if p.as_os_str().is_empty(){break;}directories.insert(p.to_str().unwrap().into());parent=p.parent();}}
    // Include parents of the required empty directories too.
    for path in REQUIRED_DIRS {let mut parent=Path::new(path).parent();while let Some(p)=parent{if p.as_os_str().is_empty(){break;}directories.insert(p.to_str().unwrap().into());parent=p.parent();}}
    let mut entries=directories.into_iter().map(|path|CompilerEntry::Directory{path,mode:0o555}).collect::<Vec<_>>();
    let mut payloads=BTreeMap::new();let mut logical_bytes=0;
    for(path,mode,bytes)in files {logical_bytes+=bytes.len()as u64;entries.push(CompilerEntry::File{path:path.clone(),mode,size_bytes:bytes.len()as u64,sha256:format!("{:x}",Sha256::digest(&bytes))});assert!(payloads.insert(path,bytes).is_none());}
    entries.sort_by(|a,b|a.path().as_bytes().cmp(b.path().as_bytes()));
    (CompilerManifest{format_version:1,profile_id:PROFILE.into(),entries,logical_bytes},payloads)
}
fn parsed(raw:&[u8])->Result<CompilerManifest>{parse_manifest(raw,&format!("{:x}",Sha256::digest(raw)),false)}
fn good()->CompilerManifest{manifest_for(vec![]).0}

#[test]
fn compiler_manifest_accepts_only_exact_canonical_pinned_schema() {
    let value=good();let raw=canonical_json(&value).unwrap();assert_eq!(parsed(&raw).unwrap(),value);
    assert!(parse_manifest(&raw,COMPILER_MANIFEST_SHA256,true).is_err());
    assert!(parse_manifest(&raw,&format!("{:x}",Sha256::digest(&raw)),true).is_err()); // fixture never qualifies A2
    let mut newline=raw.clone();newline.push(b'\n');assert!(parsed(&newline).is_err());
    let mut unknown=serde_json::to_value(&value).unwrap();unknown["runtime_grant"]=true.into();assert!(parsed(&canonical_json(&unknown).unwrap()).is_err());
    let duplicate=String::from_utf8(raw).unwrap().replacen("{","{\"format_version\":1,",1);assert!(parsed(duplicate.as_bytes()).is_err());
    let mut symlink=serde_json::to_value(&value).unwrap();symlink["entries"][0]=serde_json::json!({"kind":"symlink","path":"buck2","target":"/etc/passwd"});assert!(parsed(&canonical_json(&symlink).unwrap()).is_err());
}
#[test]
fn compiler_manifest_refuses_membership_mode_parent_count_and_path_changes() {
    let original=good();
    for bad in ["../outside","prepared//tools","/absolute","prepared/./tools","prepared/../tools","prepared/tools/","manifest.json","hardlinks.json"] {
        let mut m=original.clone();match &mut m.entries[0]{CompilerEntry::Directory{path,..}|CompilerEntry::File{path,..}=>*path=bad.into()};assert!(parsed(&canonical_json(&m).unwrap()).is_err(),"{bad}");
    }
    let mut m=original.clone();m.entries.push(m.entries.last().unwrap().clone());assert!(parsed(&canonical_json(&m).unwrap()).is_err());
    let mut m=original.clone();m.entries.retain(|e|e.path()!="prepared/tools");assert!(parsed(&canonical_json(&m).unwrap()).is_err());
    let mut m=original.clone();m.logical_bytes+=1;assert!(parsed(&canonical_json(&m).unwrap()).is_err());
    let mut m=original.clone();for e in &mut m.entries {if let CompilerEntry::File{mode,..}=e {*mode=0o755;break;}}assert!(parsed(&canonical_json(&m).unwrap()).is_err());
    let mut m=original;m.entries.retain(|e|e.path()!="prepared/proc");assert!(parsed(&canonical_json(&m).unwrap()).is_err());
}

struct Fixture {base:PathBuf,root:PathBuf,store:LocalStore,raw:Vec<u8>}
impl Fixture {
    fn new(large:bool)->Self {
        let base=Path::new("/workspace").join(format!("rudder-compiler-cas-test-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        let root=base.join("source");fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let extra=if large{vec![("prepared/tools/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-fixture.rlib".into(),0o444,vec![0x79;content::CHUNK_BYTES+31])]}else{vec![]};
        let(mut manifest,payloads)=manifest_for(extra);
        for entry in &manifest.entries {if let CompilerEntry::Directory{path,..}=entry {fs::create_dir_all(root.join(path)).unwrap();}}
        for entry in &manifest.entries {if let CompilerEntry::File{path,mode,..}=entry {fs::write(root.join(path),&payloads[path]).unwrap();fs::set_permissions(root.join(path),fs::Permissions::from_mode(*mode)).unwrap();}}
        // Genuine in-root hardlinks retain logical membership and share content.
        fs::hard_link(root.join("prepared/tools/node"),root.join("prepared/tools/node-alias")).unwrap();
        let mut alias=manifest.entries.iter().find(|e|e.path()=="prepared/tools/node").unwrap().clone();
        if let CompilerEntry::File{path,size_bytes,..}=&mut alias {*path="prepared/tools/node-alias".into();manifest.logical_bytes+=*size_bytes;}
        manifest.entries.push(alias);manifest.entries.sort_by(|a,b|a.path().as_bytes().cmp(b.path().as_bytes()));
        let raw=canonical_json(&manifest).unwrap();fs::write(root.join("manifest.json"),&raw).unwrap();fs::set_permissions(root.join("manifest.json"),fs::Permissions::from_mode(0o444)).unwrap();
        for entry in &manifest.entries {if let CompilerEntry::Directory{path,..}=entry {fs::set_permissions(root.join(path),fs::Permissions::from_mode(0o555)).unwrap();}}
        fs::set_permissions(&root,fs::Permissions::from_mode(0o555)).unwrap();let store=LocalStore::open(base.join("cas")).unwrap();Self{base,root,store,raw}
    }
    fn capture(&self,out:&mut Option<ImageDataBuild>,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ImageDataBuild::capture_compiler_fixture_retaining(out,self.store.clone(),original,cancelled,CompilerSource::fixture(self.root.clone(),&self.raw))
    }
    fn retention_data(&self)->serde_json::Value {serde_json::from_slice(&fs::read(self.store.root().join("retention.json")).unwrap()).unwrap()}
}
impl Drop for Fixture {fn drop(&mut self){let _=fs::remove_dir_all(&self.base);}}
fn root_fixture()->bool {unsafe{libc::geteuid()==0&&libc::getegid()==0}}
fn budget()->ImagePreseedBudget {ImagePreseedBudget::begin(Duration::from_secs(30)).unwrap()}
fn file_at(store:&LocalStore,mut root:ObjectRef,path:&str)->(ObjectRef,u64,u32) {
    let parts=path.split('/').collect::<Vec<_>>();for(index,name)in parts.iter().enumerate(){let node=PagedDirectory::new(&LocalDirectoryStore::read_only(store)).lookup(&root,name.as_bytes()).unwrap().unwrap().node;
        match node{EntryNode::Directory{object}if index+1<parts.len()=>root=object,EntryNode::File{object,logical_size,mode}if index+1==parts.len()=>return(object,logical_size,mode),_=>panic!("unexpected compiler node")}}
    panic!("empty compiler path")
}

#[test]
fn compiler_actual_chunk_import_preserves_layout_hashes_links_and_two_original_pins() {
    if !root_fixture(){return;} // Actual Root-owned source/CAS test, not a non-Root qualification.
    let f=Fixture::new(true);let mut out=None;f.capture(&mut out,budget(),Arc::new(AtomicBool::new(false))).unwrap();let build=out.as_ref().unwrap();let pin=build.pin_data().unwrap();
    let retained=f.store.retention().unwrap();assert_eq!(retained.pins.len(),2);assert!(retained.staging.is_empty()&&retained.image_import_holds.is_empty());
    assert!(retained.pins.values().any(|r|r==&pin.manifest)&&retained.pins.values().any(|r|r==&pin.installer_input));
    let raw_entry=file_at(&f.store,pin.installer_input.clone(),"manifest.json");assert_eq!(f.store.read_object(&raw_entry.0).unwrap(),f.raw);
    assert_eq!(file_at(&f.store,pin.installer_input.clone(),"buck2").2,0o555);
    let large=file_at(&f.store,pin.installer_input.clone(),"prepared/tools/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-fixture.rlib");assert_eq!(large.0.kind,ObjectKind::File);
    assert_eq!(content::read_file_range(&f.store,&large.0,content::CHUNK_BYTES as u64-8,39).unwrap(),vec![0x79;39]);
    let topo=file_at(&f.store,pin.installer_input.clone(),"hardlinks.json");let topology:HardlinkTopology=serde_json::from_slice(&f.store.read_object(&topo.0).unwrap()).unwrap();assert_eq!(topology.groups.len(),1);assert_eq!(topology.groups[0].paths_hex.len(),2);
    assert_eq!(file_at(&f.store,pin.installer_input.clone(),"prepared/tools/node").0,file_at(&f.store,pin.installer_input.clone(),"prepared/tools/node-alias").0);
    let binding=build.compiler_binding_data().unwrap();assert_eq!(binding.artifact_manifest,ObjectRef::from_bytes(ObjectKind::Blob,&f.raw));assert_ne!(binding.artifact_manifest.digest,COMPILER_MANIFEST_SHA256);assert_eq!(binding.artifact_image,"unqualified-component-test-fixture");
    let objects=build.objects_for_import().unwrap();assert!(objects.len()<=MAX_ENTRIES);for r in objects{assert!(r==pin.manifest||r.size_bytes<=content::CHUNK_BYTES as u64);}
    build.check_deep_for_import().unwrap();
}
#[test]
fn compiler_actual_changed_payload_retains_partial_effects_without_ack_or_pin() {
    if !root_fixture(){return;}
    let f=Fixture::new(false);let path=f.root.join("buck2");let n=fs::metadata(&path).unwrap().len()as usize;fs::write(&path,vec![b'!';n]).unwrap();
    let mut out=None;let error=f.capture(&mut out,budget(),Arc::new(AtomicBool::new(false))).unwrap_err();assert!(format!("{error:#}").contains("raw SHA"));
    let build=out.as_ref().unwrap();assert!(build.pin_data().is_err()&&build.compiler_binding_data().is_err());assert!(build.import_stats_data().unwrap().received_calls>0);
    // Inspect actual retained disk DATA without acquiring/releasing the original
    // carrier's GC lock or treating readback as an acknowledgment.
    let retained=f.retention_data();assert!(retained["state"]["pins"].as_object().unwrap().is_empty());assert_eq!(retained["state"]["image_import_holds"].as_object().unwrap().len(),1);
}
#[test]
fn compiler_actual_missing_extra_and_symlink_members_never_publish() {
    if !root_fixture(){return;}
    for case in ["missing","extra","symlink"] {let f=Fixture::new(false);
        match case{"missing"=>fs::remove_file(f.root.join("buck2")).unwrap(),"extra"=>{fs::write(f.root.join("extra"),b"undeclared").unwrap();fs::set_permissions(f.root.join("extra"),fs::Permissions::from_mode(0o444)).unwrap();},_=>{fs::remove_file(f.root.join("buck2")).unwrap();std::os::unix::fs::symlink("/etc/passwd",f.root.join("buck2")).unwrap();}}
        let mut out=None;assert!(f.capture(&mut out,budget(),Arc::new(AtomicBool::new(false))).is_err(),"{case}");assert!(out.as_ref().unwrap().pin_data().is_err());
        assert!(f.retention_data()["state"]["pins"].as_object().unwrap().is_empty());
    }
}
#[test]
fn compiler_actual_manifest_replacement_cannot_adopt_another_named_inode() {
    if !root_fixture(){return;}
    let f=Fixture::new(false);let mut source=CompilerSource::fixture(f.root.clone(),&f.raw);source.capture(&budget(),&AtomicBool::new(false)).unwrap();
    let inode=source.manifest_fd.as_ref().unwrap().metadata().unwrap().ino();let path=f.root.join("manifest.json");fs::rename(&path,f.root.join("old-manifest")).unwrap();fs::write(&path,&f.raw).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    assert!(source.check_current().is_err());assert_eq!(source.manifest_fd.as_ref().unwrap().metadata().unwrap().ino(),inode);assert_ne!(fs::metadata(path).unwrap().ino(),inode);
}
#[test]
fn compiler_actual_original_cancellation_retains_carrier_and_refuses_restart() {
    if !root_fixture(){return;}
    let f=Fixture::new(false);let original=budget();let cancelled=Arc::new(AtomicBool::new(true));let mut out=None;
    assert!(f.capture(&mut out,original.clone(),cancelled).is_err());assert!(out.is_some()&&original.remaining().is_err());assert!(out.as_ref().unwrap().pin_data().is_err());
    assert!(f.capture(&mut out,budget(),Arc::new(AtomicBool::new(false))).is_err());assert!(f.store.retention().unwrap().pins.is_empty());
}
