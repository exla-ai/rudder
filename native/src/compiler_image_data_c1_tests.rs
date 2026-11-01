//! Catalogue/real-file mechanism regressions. Small fixture bytes never qualify
//! the actual C1 artifact, SDK execution, a Root mount or a runtime role.
use super::*;
use rudder_engine::{content::{self,ContentStore},directory::{EntryNode,LocalDirectoryStore,PagedDirectory},tree::HardlinkTopology};
use std::{os::unix::fs::{DirBuilderExt,PermissionsExt},time::Duration};

const LIBRARY:&str="prepared/tools/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-c1-fixture.rlib";
fn executable(path:&str)->bool {
    (REQUIRED_FILES.contains(&path)&&path!="prepared/tools/typescript/lib/tsc.js")
        ||["prepared/tools/rust/bin/cargo","prepared/bin/sh","prepared/usr/lib/gcc/x86_64-linux-gnu/12/cc1","prepared/usr/lib/gcc/x86_64-linux-gnu/12/cc1plus","prepared/usr/lib/gcc/x86_64-linux-gnu/12/collect2"].contains(&path)
        ||C1_DRIVERS.iter().any(|driver|path==format!("prepared/usr/bin/{driver}"))
}
fn fixture_manifest(profile:CompilerProfile,large:bool)->(CompilerManifest,BTreeMap<String,Vec<u8>>) {
    let mut files: BTreeSet<String>=REQUIRED_FILES.into_iter().map(str::to_owned).collect();
    files.insert(LIBRARY.into());
    if profile==CompilerProfile::C1 {files.extend(C1_REQUIRED_FILES.iter().map(|p|p.to_string()));files.extend(C1_DRIVERS.iter().map(|p|format!("prepared/usr/bin/{p}")));}
    let mut directories:BTreeSet<String>=REQUIRED_DIRS.into_iter().map(str::to_owned).collect();
    for path in files.iter().map(String::as_str).chain(REQUIRED_DIRS) {
        let mut parent=Path::new(path).parent();while let Some(p)=parent {if p.as_os_str().is_empty(){break;}directories.insert(p.to_str().unwrap().into());parent=p.parent();}
    }
    let mut entries=directories.into_iter().map(|path|CompilerEntry::Directory{path,mode:0o555}).collect::<Vec<_>>();
    let mut payloads=BTreeMap::new();let mut logical_bytes=0;
    for path in files {
        let bytes=if large&&path==LIBRARY {vec![0x61;content::CHUNK_BYTES+31]}else{format!("unqualified C1 mechanism fixture:{path}").into_bytes()};
        logical_bytes+=bytes.len()as u64;entries.push(CompilerEntry::File{mode:if executable(&path){0o555}else{0o444},path:path.clone(),size_bytes:bytes.len()as u64,sha256:format!("{:x}",Sha256::digest(&bytes))});payloads.insert(path,bytes);
    }
    entries.sort_by(|a,b|a.path().as_bytes().cmp(b.path().as_bytes()));
    (CompilerManifest{format_version:1,profile_id:profile.id().into(),entries,logical_bytes},payloads)
}
fn parse_fixture(manifest:&CompilerManifest,profile:CompilerProfile)->Result<CompilerManifest> {
    let raw=canonical_json(manifest)?;parse_profile_manifest(&raw,&format!("{:x}",Sha256::digest(&raw)),profile,false)
}
fn without(manifest:&CompilerManifest,path:&str)->CompilerManifest {
    let mut result=manifest.clone();
    if let Some(CompilerEntry::File{size_bytes,..})=result.entries.iter().find(|e|e.path()==path){result.logical_bytes-=*size_bytes;}
    result.entries.retain(|e|e.path()!=path);result
}

#[test]
fn c1_descriptor_matches_actual_a6_data_receipt_without_claiming_execution() {
    let report:serde_json::Value=serde_json::from_str(include_str!("fixtures/compiler_c1_artifact_a6.json")).unwrap();
    assert_eq!(report["sourceManifestSha256"],COMPILER_C1_ARTIFACT_SOURCE_MANIFEST_SHA256);
    assert_eq!(report["digest"],COMPILER_C1_ARTIFACT_IMAGE.rsplit_once('@').unwrap().1);
    assert_eq!(report["exitCode"],0);assert_eq!(report["componentPassed"],true);
    assert_eq!(report["runtimeQualified"],false);assert_eq!(report["productionActivation"],false);
    for receipt in report["receipts"].as_array().unwrap() {
        assert_eq!(receipt["profile_id"],CompilerProfile::C1.id());assert_eq!(receipt["manifest_sha256"],CompilerProfile::C1.manifest());
        assert_eq!(receipt["entries"],C1_ENTRIES as u64);assert_eq!(receipt["logical_bytes"],C1_BYTES);
        assert_eq!(receipt["execution_qualified"],false);assert_eq!(receipt["sdk_grant"],false);
    }
    assert_eq!(CompilerProfile::C1.source(),"/rudder-compiler-c1");assert_eq!(CompilerProfile::C1.counts(),(6108,1_372_802_439));
    assert_ne!(CompilerProfile::A2.manifest(),CompilerProfile::C1.manifest());assert_ne!(CompilerProfile::A2.image(),CompilerProfile::C1.image());
}
#[test]
fn c1_synthetic_catalogue_is_not_the_fixed_c1_or_a2_artifact() {
    let(manifest,_)=fixture_manifest(CompilerProfile::C1,false);let raw=canonical_json(&manifest).unwrap();let fixture_hash=format!("{:x}",Sha256::digest(&raw));
    assert_eq!(parse_fixture(&manifest,CompilerProfile::C1).unwrap(),manifest);
    assert!(parse_profile_manifest(&raw,COMPILER_C1_MANIFEST_SHA256,CompilerProfile::C1,true).is_err());
    assert!(parse_profile_manifest(&raw,&fixture_hash,CompilerProfile::C1,true).is_err());
    assert!(parse_profile_manifest(&raw,&fixture_hash,CompilerProfile::A2,false).is_err());
    let mut a2=manifest.clone();a2.profile_id=PROFILE.into();assert!(parse_fixture(&a2,CompilerProfile::C1).is_err());
}
#[test]
fn c1_requires_cargo_all_drivers_cxx_headers_and_crt() {
    let(manifest,_)=fixture_manifest(CompilerProfile::C1,false);
    for path in C1_REQUIRED_FILES.iter().map(|p|p.to_string()).chain(C1_DRIVERS.iter().map(|p|format!("prepared/usr/bin/{p}"))) {
        assert!(parse_fixture(&without(&manifest,&path),CompilerProfile::C1).is_err(),"{path}");
    }
    for path in REQUIRED_DIRS {assert!(parse_fixture(&without(&manifest,path),CompilerProfile::C1).is_err(),"{path}");}
}
#[test]
fn c1_requires_exact_executable_modes_but_keeps_headers_and_typescript_readonly() {
    let(manifest,_)=fixture_manifest(CompilerProfile::C1,false);
    assert!(matches!(manifest.entries.iter().find(|e|e.path()=="prepared/tools/typescript/lib/tsc.js").unwrap(),CompilerEntry::File{mode:0o444,..}));
    for path in ["prepared/tools/rust/bin/cargo","prepared/usr/bin/gcc","prepared/usr/bin/g++","prepared/usr/lib/gcc/x86_64-linux-gnu/12/cc1plus","prepared/bin/sh"] {
        let mut changed=manifest.clone();if let CompilerEntry::File{mode,..}=changed.entries.iter_mut().find(|e|e.path()==path).unwrap(){*mode=0o444;}
        assert!(parse_fixture(&changed,CompilerProfile::C1).is_err(),"{path}");
    }
}
#[test]
fn c1_refuses_noncanonical_unknown_unsafe_and_incomplete_manifest_data() {
    let(manifest,_)=fixture_manifest(CompilerProfile::C1,false);let raw=canonical_json(&manifest).unwrap();
    let parsed=|bytes:&[u8]|parse_profile_manifest(bytes,&format!("{:x}",Sha256::digest(bytes)),CompilerProfile::C1,false);
    let mut newline=raw.clone();newline.push(b'\n');assert!(parsed(&newline).is_err());
    let duplicate=String::from_utf8(raw).unwrap().replacen('{',"{\"format_version\":1,",1);assert!(parsed(duplicate.as_bytes()).is_err());
    let mut unknown=serde_json::to_value(&manifest).unwrap();unknown["sdk_grant"]=true.into();assert!(parsed(&canonical_json(&unknown).unwrap()).is_err());
    let mut symlink=serde_json::to_value(&manifest).unwrap();symlink["entries"][0]=serde_json::json!({"kind":"symlink","path":"buck2","target":"/etc/passwd"});assert!(parsed(&canonical_json(&symlink).unwrap()).is_err());
    for path in ["../outside","/absolute","prepared//bad","prepared/../bad","manifest.json","hardlinks.json"] {
        let mut changed=manifest.clone();if let CompilerEntry::File{path:p,..}=&mut changed.entries[0]{*p=path.into();}else{panic!("buck2 must be first file");}
        assert!(parse_fixture(&changed,CompilerProfile::C1).is_err(),"{path}");
    }
    let mut changed=manifest;changed.logical_bytes+=1;assert!(parse_fixture(&changed,CompilerProfile::C1).is_err());
}
#[test]
fn a2_parser_and_fixed_descriptor_remain_distinct_and_compatible() {
    let(manifest,_)=fixture_manifest(CompilerProfile::A2,false);let raw=canonical_json(&manifest).unwrap();
    assert_eq!(parse_manifest(&raw,&format!("{:x}",Sha256::digest(&raw)),false).unwrap(),manifest);
    assert!(parse_fixture(&manifest,CompilerProfile::C1).is_err());
    assert_eq!(CompilerProfile::A2.source(),"/rudder-compiler-v1");assert_eq!(CompilerProfile::A2.counts(),(1047,852_319_618));
    assert_eq!(COMPILER_CAS_DIRECTORY,"/var/lib/rudder-compiler-cas-v1");assert_eq!(CompilerProfile::A2.id(),"rudder-linux-rust-typescript-v1");
}
#[test]
fn c1_keeps_original_two_gib_65k_and_manifest_bounds() {
    assert_eq!(MAX_BYTES,2*1024*1024*1024);assert_eq!(MAX_ENTRIES,65_536);assert_eq!(MAX_MANIFEST,32*1024*1024);
    let(mut manifest,_)=fixture_manifest(CompilerProfile::C1,false);
    if let CompilerEntry::File{size_bytes,..}=&mut manifest.entries[0]{manifest.logical_bytes=manifest.logical_bytes-*size_bytes+MAX_BYTES+1;*size_bytes=MAX_BYTES+1;}
    assert!(parse_fixture(&manifest,CompilerProfile::C1).is_err());
    assert!(parse_profile_manifest(&[],COMPILER_C1_MANIFEST_SHA256,CompilerProfile::C1,true).is_err());
}

struct Fixture{base:PathBuf,root:PathBuf,store:LocalStore,raw:Vec<u8>}
impl Fixture {
    fn new(large:bool)->Self {
        assert!(unsafe{libc::getuid()==0&&libc::geteuid()==0&&libc::getgid()==0&&libc::getegid()==0},"actual Root-only fixture");
        let base=Path::new("/var/lib").join(format!("rudder-c1-cas-mechanism-test-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        let root=base.join("source");fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let(mut manifest,payloads)=fixture_manifest(CompilerProfile::C1,large);
        for entry in &manifest.entries{if let CompilerEntry::Directory{path,..}=entry{fs::create_dir_all(root.join(path)).unwrap();}}
        for entry in &manifest.entries{if let CompilerEntry::File{path,mode,..}=entry{fs::write(root.join(path),&payloads[path]).unwrap();fs::set_permissions(root.join(path),fs::Permissions::from_mode(*mode)).unwrap();}}
        fs::hard_link(root.join("prepared/tools/node"),root.join("prepared/tools/node-alias")).unwrap();
        let mut alias=manifest.entries.iter().find(|e|e.path()=="prepared/tools/node").unwrap().clone();if let CompilerEntry::File{path,size_bytes,..}=&mut alias{*path="prepared/tools/node-alias".into();manifest.logical_bytes+=*size_bytes;}
        manifest.entries.push(alias);manifest.entries.sort_by(|a,b|a.path().as_bytes().cmp(b.path().as_bytes()));let raw=canonical_json(&manifest).unwrap();
        fs::write(root.join("manifest.json"),&raw).unwrap();fs::set_permissions(root.join("manifest.json"),fs::Permissions::from_mode(0o444)).unwrap();
        for entry in &manifest.entries{if let CompilerEntry::Directory{path,..}=entry{fs::set_permissions(root.join(path),fs::Permissions::from_mode(0o555)).unwrap();}}
        fs::set_permissions(&root,fs::Permissions::from_mode(0o555)).unwrap();let store=LocalStore::open(base.join("cas")).unwrap();Self{base,root,store,raw}
    }
    fn capture(&self,out:&mut Option<ImageDataBuild>,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ImageDataBuild::capture_compiler_fixture_retaining(out,self.store.clone(),original,cancelled,CompilerSource::fixture_c1(self.root.clone(),&self.raw))
    }
    fn retained_data(&self)->serde_json::Value{serde_json::from_slice(&fs::read(self.store.root().join("retention.json")).unwrap()).unwrap()}
}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.base);}}
fn original()->ImagePreseedBudget{ImagePreseedBudget::begin(Duration::from_secs(30)).unwrap()}
fn file_at(store:&LocalStore,mut root:ObjectRef,path:&str)->(ObjectRef,u64,u32){
    let parts=path.split('/').collect::<Vec<_>>();for(index,name)in parts.iter().enumerate(){let node=PagedDirectory::new(&LocalDirectoryStore::read_only(store)).lookup(&root,name.as_bytes()).unwrap().unwrap().node;
        match node{EntryNode::Directory{object}if index+1<parts.len()=>root=object,EntryNode::File{object,logical_size,mode}if index+1==parts.len()=>return(object,logical_size,mode),_=>panic!("unexpected C1 node")}}
    panic!("empty C1 path")
}
#[test]
#[ignore="actual Root-owned compiler DATA files/CAS, no SDK execution or mount qualification"]
fn c1_actual_import_keeps_complete_manifest_chunks_hardlinks_and_two_original_pins(){
    let f=Fixture::new(true);let mut out=None;f.capture(&mut out,original(),Arc::new(AtomicBool::new(false))).unwrap();let build=out.as_ref().unwrap();let pin=build.pin_data().unwrap();
    let retained=f.store.retention().unwrap();assert_eq!(retained.pins.len(),2);assert!(retained.staging.is_empty()&&retained.image_import_holds.is_empty());
    assert!(retained.pins.values().any(|r|r==&pin.manifest)&&retained.pins.values().any(|r|r==&pin.installer_input));
    let raw_manifest=file_at(&f.store,pin.installer_input.clone(),"manifest.json");assert_eq!(f.store.read_object(&raw_manifest.0).unwrap(),f.raw);
    for path in ["prepared/usr/bin/gcc","prepared/usr/bin/g++","prepared/tools/rust/bin/cargo"]{assert_eq!(file_at(&f.store,pin.installer_input.clone(),path).2,0o555);}
    assert_eq!(file_at(&f.store,pin.installer_input.clone(),"prepared/usr/include/stdio.h").2,0o444);
    let library=file_at(&f.store,pin.installer_input.clone(),LIBRARY);assert_eq!(library.0.kind,ObjectKind::File);assert_eq!(content::read_file_range(&f.store,&library.0,content::CHUNK_BYTES as u64-8,39).unwrap(),vec![0x61;39]);
    let topology=file_at(&f.store,pin.installer_input.clone(),"hardlinks.json");let links:HardlinkTopology=serde_json::from_slice(&f.store.read_object(&topology.0).unwrap()).unwrap();assert_eq!(links.groups.len(),1);assert_eq!(links.groups[0].paths_hex.len(),2);
    let binding=build.compiler_binding_data().unwrap();assert_eq!(binding.profile_id,COMPILER_C1_PROFILE);assert_eq!(binding.artifact_manifest,ObjectRef::from_bytes(ObjectKind::Blob,&f.raw));assert_eq!(binding.artifact_image,"unqualified-component-test-fixture");assert_ne!(binding.artifact_manifest.digest,COMPILER_C1_MANIFEST_SHA256);
    let objects=build.objects_for_import().unwrap();assert!(objects.contains(&raw_manifest.0));assert!(objects.contains(&pin.installer_input)&&objects.contains(&pin.manifest));
    for reference in objects{assert!(reference==pin.manifest||reference.size_bytes<=content::CHUNK_BYTES as u64);}build.check_deep_for_import().unwrap();
}
#[test]
#[ignore="actual Root-owned compiler DATA files/CAS, no SDK execution or mount qualification"]
fn c1_actual_outer_root0755_refuses_before_payload_import_or_pin(){
    let f=Fixture::new(false);fs::set_permissions(&f.root,fs::Permissions::from_mode(0o755)).unwrap();let mut out=None;
    let error=f.capture(&mut out,original(),Arc::new(AtomicBool::new(false))).unwrap_err();assert!(format!("{error:#}").contains("compiler root mode differs"));
    assert!(out.as_ref().unwrap().pin_data().is_err());let retained=f.store.retention().unwrap();assert!(retained.pins.is_empty()&&retained.staging.is_empty()&&retained.image_import_holds.is_empty());
}
#[test]
#[ignore="actual Root-owned compiler DATA files/CAS, no SDK execution or mount qualification"]
fn c1_actual_same_length_payload_corruption_retains_effects_without_ack_or_new_attempt(){
    let f=Fixture::new(false);let path=f.root.join("prepared/usr/bin/gcc");let size=fs::metadata(&path).unwrap().len()as usize;fs::write(&path,vec![b'!';size]).unwrap();
    let mut out=None;let error=f.capture(&mut out,original(),Arc::new(AtomicBool::new(false))).unwrap_err();assert!(format!("{error:#}").contains("raw SHA"));
    let build=out.as_ref().unwrap();assert!(build.pin_data().is_err()&&build.compiler_binding_data().is_err());assert!(build.import_stats_data().unwrap().received_calls>0);
    let retained=f.retained_data();assert!(retained["state"]["pins"].as_object().unwrap().is_empty());assert_eq!(retained["state"]["image_import_holds"].as_object().unwrap().len(),1);
    assert!(f.capture(&mut out,original(),Arc::new(AtomicBool::new(false))).is_err());
}
#[test]
#[ignore="actual Root-owned compiler DATA files/CAS, no SDK execution or mount qualification"]
fn c1_actual_manifest_replacement_keeps_original_fd_and_cancellation_keeps_carrier(){
    let f=Fixture::new(false);let mut source=CompilerSource::fixture_c1(f.root.clone(),&f.raw);source.capture(&original(),&AtomicBool::new(false)).unwrap();
    let original_inode=source.manifest_fd.as_ref().unwrap().metadata().unwrap().ino();let path=f.root.join("manifest.json");fs::rename(&path,f.root.join("old-manifest")).unwrap();fs::write(&path,&f.raw).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    assert!(source.check_current().is_err());assert_eq!(source.manifest_fd.as_ref().unwrap().metadata().unwrap().ino(),original_inode);
    let cancelled=original();let mut out=None;assert!(f.capture(&mut out,cancelled.clone(),Arc::new(AtomicBool::new(true))).is_err());
    assert!(out.is_some()&&cancelled.remaining().is_err());assert!(out.as_ref().unwrap().pin_data().is_err());assert!(f.store.retention().unwrap().pins.is_empty());
}
