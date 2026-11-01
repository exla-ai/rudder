//! Root-only Linux storage/jj COMPONENT tests. These use the strict production
//! baked descriptor reader and real preparation/retention APIs. They issue no
//! RuntimeImageClient, Root mount, PG, task, SDK or execution authority.
#![cfg(all(feature="jj",target_os="linux"))]
mod common;
use anyhow::Result;
use gix::{objs::{Kind,Write as _},ObjectId};
use rudder_engine::{build::{BuildIndex,PreparationSpec,PreparedContentLayout,PlatformIdentity,ToolchainIdentity,RetainedBackedPreparation},
    content::{ContentStore,FileNode,FILE_LAYOUT},directory::{DirectoryEntry,EntryNode,PagedDirectory},
    execution::finalization_budget::FinalizationBudget,protocol::{canonical_json,ObjectKind,ObjectRef},
    snapshot::{decode_runnable,decode_source},store::{LocalStore,LocalImageImport,ImageImportFence,
        baked_image::{BakedImageCas,BakedImageInventoryData,BakedImageRoots,BAKED_IMAGE_LAYOUT},
        backed_inputs::VerifiedBakedInputs,backed_retention::BackedRead}};
use rudder_vcs_bridge::{GitCommitId,initial_source::{InitialSourceRequest,PreparedInitialSource,
    backed::{BackedInitialEnvironment,prepare_initial_source_backed_retaining}},seed::SeedProfile};
use sha2::{Digest,Sha256};
use std::{cell::RefCell,collections::{BTreeMap,BTreeSet},fs::{self,OpenOptions},os::{fd::OwnedFd,unix::fs::{OpenOptionsExt,PermissionsExt}},
    path::{Path,PathBuf},sync::{Arc,atomic::{AtomicBool,Ordering}},time::Duration};
struct Clock(FinalizationBudget);
impl ImageImportFence for Clock{fn remaining(&self)->Result<Duration>{self.0.remaining()}}
fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn clock()->Arc<Clock>{Arc::new(Clock(budget()))}
struct Fixture {temporary:tempfile::TempDir,baked_root:PathBuf,source:gix::Repository,store:LocalStore,
    inputs:Arc<VerifiedBakedInputs>,preparation:RetainedBackedPreparation,private:PathBuf,seed:PathBuf,head:GitCommitId,
    payload:ObjectRef,bytes:Vec<u8>,_import:rudder_engine::build::index_acquisition::RetainedIndexAcquisition,
    git_import:RefCell<Option<rudder_vcs_bridge::accepted_git_import::RetainedGitImport>>,paged:RefCell<Option<rudder_vcs_bridge::seed::PagedSeedGeneration>>}
impl Fixture {
    fn new()->Self {
        assert_eq!(unsafe{libc::geteuid()},0,"strict production Root-owned DATA fixture requires UID0");
        let temporary=tempfile::tempdir_in("/workspace").unwrap();fs::set_permissions(temporary.path(),fs::Permissions::from_mode(0o700)).unwrap();
        let root=temporary.path().canonicalize().unwrap();let baked_root=root.join("baked");fs::create_dir(&baked_root).unwrap();
        let backing=LocalStore::open(baked_root.join("cas")).unwrap();let mut imported=None;
        LocalImageImport::stage_retaining(&mut imported,backing.clone(),clock()).unwrap();let imported=imported.unwrap();
        let bytes=vec![b'R';4*1024*1024];let payload=ContentStore::put_blob(&imported,&bytes).unwrap();
        let tail=ContentStore::put_blob(&imported,b"component-runtime-tail").unwrap();
        let file=ContentStore::put_file_node(&imported,&FileNode{format_version:1,layout:FILE_LAYOUT.into(),level:0,
            logical_size:payload.size_bytes+tail.size_bytes,children:vec![payload.clone(),tail.clone()]}).unwrap();
        let topology=ContentStore::put_blob(&imported,b"{\"format_version\":1,\"groups\":[]}").unwrap();
        let installer=PagedDirectory::new(&imported).build(0o555,vec![
            DirectoryEntry::new(b"hardlinks.json",EntryNode::File{object:topology.clone(),logical_size:topology.size_bytes,mode:0o444}).unwrap(),
            DirectoryEntry::new(b"runtime",EntryNode::File{object:file.clone(),logical_size:payload.size_bytes+tail.size_bytes,mode:0o555}).unwrap()]).unwrap();
        let mut sha=Sha256::new();sha.update(&bytes);sha.update(b"component-runtime-tail");
        let raw=canonical_json(&serde_json::json!({"format_version":1,"installer_input":installer,"files":[
            {"path":"/runtime","sha256":hex::encode(sha.finalize()),"size_bytes":payload.size_bytes+tail.size_bytes,"content":file}]})).unwrap();
        let manifest=ObjectRef::from_bytes(ObjectKind::Blob,&raw);imported.receive_manifest(&manifest,&raw).unwrap();
        let pins=imported.commit_pins("component-runtime",&installer,"component-manifest",&manifest).unwrap();
        let objects=imported.objects_after_commit(&pins).unwrap();let retention=backing.retention().unwrap();
        let roots=BakedImageRoots{format_version:1,manifest:manifest.clone(),installer_input:installer.clone()};
        let inventory=BakedImageInventoryData{format_version:1,layout:BAKED_IMAGE_LAYOUT.into(),roots:roots.clone(),
            total_bytes:objects.iter().map(|r|r.size_bytes).sum(),objects,retention:ObjectRef::from_bytes(ObjectKind::Blob,&fs::read(baked_root.join("cas/retention.json")).unwrap()),
            retention_generation:retention.generation,pins:retention.pins};
        let inventory_bytes=canonical_json(&inventory).unwrap();let expected=ObjectRef::from_bytes(ObjectKind::Blob,&inventory_bytes);
        fs::write(baked_root.join("inventory.json"),inventory_bytes).unwrap();fs::write(baked_root.join("pin.json"),canonical_json(&roots).unwrap()).unwrap();
        fs::write(baked_root.join("manifest.json"),raw).unwrap();drop(imported);
        for name in ["inventory.json","pin.json","manifest.json","cas/lock","cas/retention.json"]{fs::set_permissions(baked_root.join(name),fs::Permissions::from_mode(0o444)).unwrap();}
        for shard in fs::read_dir(baked_root.join("cas/objects")).unwrap(){fs::set_permissions(shard.unwrap().path(),fs::Permissions::from_mode(0o555)).unwrap();}
        for name in ["cas/objects","cas/tmp","cas",""]{fs::set_permissions(baked_root.join(name),fs::Permissions::from_mode(0o555)).unwrap();}
        let directory:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&baked_root).unwrap().into();
        let inventory_fd:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(baked_root.join("inventory.json")).unwrap().into();
        let mut baked=None;BakedImageCas::capture_retaining(&mut baked,directory,inventory_fd,expected,clock()).unwrap();
        let mut inputs=None;VerifiedBakedInputs::capture_retaining(&mut inputs,baked.unwrap(),clock()).unwrap();let inputs=inputs.unwrap();
        let store=LocalStore::open(root.join("local")).unwrap();let network=store.put_blob(b"real public network fixture DATA").unwrap();store.pin("network",&network).unwrap();
        let spec=PreparationSpec{format_version:1,base_runtime:installer.clone(),
            platform:PlatformIdentity{os:"linux".into(),architecture:std::env::consts::ARCH.into(),abi:"component-only".into(),properties:BTreeMap::new()},
            installer_inputs:BTreeMap::from([("image/installer".into(),installer.clone()),("image/manifest".into(),manifest),("vm/network".into(),network.clone())]),
            toolchains:BTreeMap::from([("runtime".into(),ToolchainIdentity{version:"component-raw-bytes".into(),closure:installer.clone(),executable:file,executable_path:"runtime".into(),configuration:BTreeMap::new()})]),
            installer_commands:Vec::new(),installer_environment:BTreeMap::new(),policy_version:"component-backed-initial-v1".into(),effects:BTreeSet::new()};
        let original=budget();let mut acquired=None;BuildIndex::create_shared_retaining(&mut acquired,root.join("index"),store.clone(),original.clone(),Arc::new(AtomicBool::new(false)),Arc::new(Clock(original))).unwrap();
        let index=acquired.as_ref().unwrap().index().unwrap();let mut preparation=None;
        index.prepare_backed_retaining(&mut preparation,spec,PreparedContentLayout::PagedChunkedV2,installer,vec![network],vec![inputs.clone()],None,budget(),Arc::new(AtomicBool::new(false))).unwrap();
        let source=gix::init_bare(root.join("source.git")).unwrap();
        let raw=|kind,bytes:&[u8]|source.objects.write_stream(kind,bytes.len()as u64,&mut &bytes[..]).unwrap();
        let blob=raw(Kind::Blob,b"accepted immutable source\n");let mut tree=b"100644 tracked\0".to_vec();tree.extend_from_slice(blob.as_bytes());let tree=raw(Kind::Tree,&tree);
        let commit=|parent:Option<ObjectId>|{let mut text=format!("tree {tree}\n");if let Some(id)=parent{text.push_str(&format!("parent {id}\n"));}
            text.push_str("author A <a@invalid.example> 1 +0000\ncommitter A <a@invalid.example> 1 +0000\n\naccepted source\n");raw(Kind::Commit,text.as_bytes())};
        let parent=commit(None);let head=commit(Some(parent));fs::write(source.path().join("HEAD"),format!("{head}\n")).unwrap();
        let private=root.join("private");fs::create_dir(&private).unwrap();fs::set_permissions(&private,fs::Permissions::from_mode(0o700)).unwrap();
        Self{temporary,baked_root,source,store,inputs,preparation:preparation.unwrap(),private,seed:root.join("seed"),head:GitCommitId(head.to_string()),payload,bytes,_import:acquired.unwrap(),git_import:RefCell::new(None),paged:RefCell::new(None)}
    }
    fn context(&self,original:&FinalizationBudget,cancel:&Arc<AtomicBool>)->BackedInitialEnvironment{
        let ack=self.preparation.acknowledgment().unwrap();let mut out=None;
        BackedInitialEnvironment::stage_retaining(&mut out,ack.prepared().root.clone(),ack.retention().clone(),vec![self.inputs.clone()],"component-backed-runnable".into(),original.clone(),cancel.clone()).unwrap();out.unwrap()
    }
    fn run(&self,context:&mut BackedInitialEnvironment,original:&FinalizationBudget,cancel:Arc<AtomicBool>,check:impl FnMut()->Result<()>)->Result<PreparedInitialSource>{
        let mut paged=self.paged.borrow_mut();let mut import=self.git_import.borrow_mut();
        pollster::block_on(prepare_initial_source_backed_retaining(self.store.clone(),InitialSourceRequest{source_git:self.source.path(),private_repository:&self.private,
            seed_destination:&self.seed,expected_head:&self.head,environment:&self.preparation.acknowledgment().unwrap().prepared().root,settings:&common::settings()},
            SeedProfile::SmallV1,&mut paged,&mut import,context,original,cancel,check))
    }
}
impl Drop for Fixture{fn drop(&mut self){fn writable(path:&Path){let Ok(m)=fs::symlink_metadata(path)else{return;};if !m.is_dir()||m.file_type().is_symlink(){return}let _=fs::set_permissions(path,fs::Permissions::from_mode(0o700));if let Ok(entries)=fs::read_dir(path){for entry in entries.flatten(){writable(&entry.path())}}}writable(&self.baked_root);let _=&self.temporary;}}

#[test]#[ignore="strict Root-owned baked DATA + real jj initial source component; no mount/PG authority"]
fn real_backed_preparation_initial_jj_source_and_seed_retain_foreign_graph_without_copy(){
    let f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(false));let mut context=f.context(&original,&cancel);
    let prepared=f.run(&mut context,&original,cancel,||Ok(())).unwrap();assert_eq!(prepared.baseline.commit,f.head);assert_eq!(prepared.baseline.parents.len(),1);
    assert_eq!(prepared.verified.receipt().source,prepared.source_root);assert!(prepared.runnable_backing().is_some());
    let owner=prepared.runnable_backing().unwrap();assert!(Arc::ptr_eq(owner,context.runnable_owner().unwrap()));
    let mut read:Option<BackedRead>=None;owner.read_retaining(&mut read,prepared.runnable_root.clone(),clock()).unwrap();
    let root=decode_runnable(read.as_ref().unwrap().verified_bytes().unwrap()).unwrap();assert_eq!(root.environment,Some(f.preparation.acknowledgment().unwrap().prepared().root.clone()));
    let source=decode_source(&f.store.read_object(&root.source).unwrap()).unwrap();assert_eq!(source.directory,root.view_root);
    assert!(f.seed.join("metadata/.git/HEAD").is_file());assert!(!f.store.object_path(&f.payload).unwrap().exists());
    assert!(!f.store.retention().unwrap().pins.values().any(|r|r==&prepared.runnable_root));assert_eq!(owner.root_data(),&prepared.runnable_root);
    let mut payload=None;owner.read_retaining(&mut payload,f.payload.clone(),clock()).unwrap();assert_eq!(payload.as_ref().unwrap().verified_bytes().unwrap(),f.bytes);
    assert!(f.store.gc(Duration::ZERO).is_err());
}
#[test]#[ignore="strict Root-owned baked DATA; original cancellation never starts jj"]
fn backed_initial_cancel_stages_original_context_and_never_initializes_private_history(){
    let f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(false));let mut context=f.context(&original,&cancel);cancel.store(true,Ordering::Release);
    assert!(f.run(&mut context,&original,cancel,||Ok(())).is_err());assert!(!f.private.join(".jj").exists());assert!(!f.seed.exists());assert!(context.runnable_owner().is_err());
}
#[test]#[ignore="strict Root-owned baked DATA; late original publication ACK remains retained"]
fn backed_initial_late_cancellation_preserves_actual_runnable_ack_without_source_success(){
    let f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(false));let mut context=f.context(&original,&cancel);let marker=cancel.clone();
    assert!(f.run(&mut context,&original,cancel,||{if f.store.retention()?.backed_retention.as_ref().is_some_and(|d|d.pins.contains_key("component-backed-runnable")){marker.store(true,Ordering::Release);}Ok(())}).is_err());
    assert!(original.remaining().is_err());assert!(context.runnable_owner().is_ok());assert!(!f.seed.exists());assert!(!f.store.object_path(&f.payload).unwrap().exists());
    assert!(f.store.gc(Duration::ZERO).is_err());
}
#[test]#[ignore="strict Root-owned baked DATA; corrupt original must never become local fallback"]
fn backed_initial_refuses_corrupted_backing_and_keeps_original_publication_guard(){
    let f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(false));let mut context=f.context(&original,&cancel);
    let path=f.baked_root.join("cas/objects").join(&f.payload.digest[..2]).join(&f.payload.digest[2..]);
    fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'X';f.bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    assert!(f.run(&mut context,&original,cancel,||Ok(())).is_err());assert!(context.runnable_owner().is_err());assert!(!f.seed.exists());
    assert!(!f.store.object_path(&f.payload).unwrap().exists());assert!(f.store.gc(Duration::ZERO).is_err());
}
