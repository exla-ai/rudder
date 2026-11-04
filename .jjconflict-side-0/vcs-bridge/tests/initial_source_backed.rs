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

// These remain storage/jj component tests. No fixture path or canonical tree
// grants Native source, project, task, mount or Journal acceptance authority.
fn source_parent_context(f:&Fixture,original:&FinalizationBudget,cancel:&Arc<AtomicBool>)
    ->(Arc<rudder_engine::store::environment_dependencies::RetainedEnvironmentSubgraph>,BackedInitialEnvironment) {
    let mut environment=None;f.preparation.derive_environment_subgraph_retaining(&mut environment,Arc::new(Clock(original.clone()))).unwrap();
    let environment=environment.unwrap();let mut context=None;
    BackedInitialEnvironment::stage_subgraph_retaining(&mut context,environment.clone(),"component-dependent-runnable".into(),original.clone(),cancel.clone()).unwrap();
    (environment,context.unwrap())
}
fn changed_source_candidate(f:&Fixture,prepared:&PreparedInitialSource,bytes:&[u8])->(ObjectRef,ObjectRef) {
    use rudder_engine::{protocol::CheckpointConsistency,snapshot::{RunnableRoot,SourceV2}};
    let base=decode_runnable(&f.store.read_object(&prepared.runnable_root).unwrap()).unwrap();
    let source=decode_source(&f.store.read_object(&base.source).unwrap()).unwrap();let blob=f.store.put_blob(bytes).unwrap();
    let stage=f.store.begin_staging(Duration::from_secs(120)).unwrap();
    let adapter=rudder_engine::directory::LocalDirectoryStore::new(&f.store,&stage.id);let pages=PagedDirectory::new(&adapter);
    let inherited=pages.lookup(&source.directory,b"tracked").unwrap().unwrap();
    let directory=pages.build(0o755,vec![inherited,DirectoryEntry::new(b"private",EntryNode::File{object:blob.clone(),logical_size:blob.size_bytes,mode:0o644}).unwrap()]).unwrap();
    let source=f.store.put_metadata(ObjectKind::SourceRoot,&SourceV2{format_version:2,consistency:CheckpointConsistency::Quiesced,directory:directory.clone(),projection_policy:source.projection_policy}).unwrap();
    let runnable=f.store.put_metadata(ObjectKind::RunnableRoot,&RunnableRoot{source:source.clone(),view_root:directory,..base}).unwrap();(source,runnable)
}
fn publish_component_overlay(f:&Fixture,prepared:&PreparedInitialSource,runnable:ObjectRef,pin:&str)
    ->rudder_engine::store::environment_dependencies::SourceOverlayPublication {
    let stage=f.store.begin_staging(Duration::from_secs(120)).unwrap();let mut out=None;
    f.store.publish_source_overlay_retaining(&mut out,stage.id,pin.into(),runnable,prepared.source_parent().unwrap().clone(),clock()).unwrap();out.unwrap()
}

#[test]#[ignore="strict Root-owned baked DATA + real initial source; shared source parent component"]
fn completed_flat_source_parent_is_shared_by_siblings_after_its_old_clock_expires(){
    let f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(false));let (environment,mut context)=source_parent_context(&f,&original,&cancel);
    let prepared=f.run(&mut context,&original,cancel,||Ok(())).unwrap();let parent=prepared.source_parent().unwrap();
    assert!(Arc::ptr_eq(parent,&context.source_parent().unwrap().unwrap()));
    assert!(Arc::ptr_eq(parent.original_owner(),prepared.runnable_dependency().unwrap()));
    assert!(Arc::ptr_eq(parent.original_owner().environment(),&environment));
    original.cancel();assert!(original.remaining().is_err());
    let (_,one)=changed_source_candidate(&f,&prepared,b"first private sibling\n");let one=publish_component_overlay(&f,&prepared,one,"component-sibling-one");
    let (_,two)=changed_source_candidate(&f,&prepared,b"second private sibling\n");let two=publish_component_overlay(&f,&prepared,two,"component-sibling-two");
    assert!(Arc::ptr_eq(one.acknowledgment().unwrap().source_parent(),parent));
    assert!(Arc::ptr_eq(two.acknowledgment().unwrap().source_parent(),parent));
    assert_eq!(one.acknowledgment().unwrap().record_data().depth,3);assert_eq!(two.acknowledgment().unwrap().record_data().depth,3);
    let mut read=None;one.acknowledgment().unwrap().read_retaining(&mut read,one.acknowledgment().unwrap().root_data().clone(),clock()).unwrap();read.as_ref().unwrap().verified_bytes().unwrap();
    assert!(!f.store.object_path(&f.payload).unwrap().exists());assert!(f.store.gc(Duration::ZERO).is_err());
}

#[test]#[ignore="strict Root-owned baked DATA + actual jj child; full accepted-overlay flatten component"]
fn accepted_overlay_is_fully_published_before_a_distinct_flat_successor_parent(){
    use jj_lib::{backend::CommitId,op_store::RefTarget,ref_name::{RefName,WorkspaceName},repo::Repo};
    use rudder_vcs_bridge::{initial_source::AcceptedSourceGeneration,projection::{project_source,verify_versioned_source},projection_io::ProjectionBudget,publication::INTEGRATION_BOOKMARK};
    let f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(false));let (environment,mut context)=source_parent_context(&f,&original,&cancel);
    let prepared=f.run(&mut context,&original,cancel,||Ok(())).unwrap();let old_parent=prepared.source_parent().unwrap().clone();
    let (source,runnable)=changed_source_candidate(&f,&prepared,b"actually accepted child source\n");let overlay=publish_component_overlay(&f,&prepared,runnable.clone(),"component-accepted-overlay");
    let repo=prepared.repository();let previous=repo.store().get_commit(&CommitId::new(hex::decode(&prepared.baseline.commit.0).unwrap())).unwrap();
    let source_value=decode_source(&f.store.read_object(&source).unwrap()).unwrap();let stage=f.store.begin_staging(Duration::from_secs(120)).unwrap();
    let projected=pollster::block_on(project_source(f.store.clone(),&source_value.directory,&previous.tree(),&source_value.projection_policy,None,None,ProjectionBudget::default())).unwrap();
    let verified=Arc::new(pollster::block_on(verify_versioned_source(f.store.clone(),&stage.id,&source,&source_value.projection_policy,&projected.tree,Some(&projected.record),ProjectionBudget::default())).unwrap());
    let mut tx=repo.start_transaction();let commit=pollster::block_on(tx.repo_mut().new_commit(vec![previous.id().clone()],projected.tree).write()).unwrap();
    tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(),commit.id().clone()).unwrap();
    tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK),RefTarget::normal(commit.id().clone()));
    let accepted=pollster::block_on(tx.commit("actual overlay component accepted operation")).unwrap();
    original.cancel();let original=budget();let cancel=Arc::new(AtomicBool::new(false));let mut successor=None;
    AcceptedSourceGeneration::prepare_overlay_retaining(&mut successor,f.store.clone(),accepted,verified,prepared.seed_cache().clone(),source.clone(),overlay.acknowledgment().unwrap().clone(),
        prepared.baseline.clone(),prepared.seed_root.clone(),"component-flat-successor".into(),"component-successor-seed".into(),original,cancel,||Ok(())).unwrap();
    let mut next=None;successor.as_mut().unwrap().take_prepared_retaining(&mut next).unwrap();let next=next.unwrap();let next_parent=next.source_parent().unwrap();let flat=next.runnable_dependency().unwrap();
    assert!(!Arc::ptr_eq(next_parent,&old_parent));assert!(Arc::ptr_eq(next_parent.original_owner(),flat));assert!(Arc::ptr_eq(flat.environment(),&environment));
    assert_eq!(next.runnable_root,runnable);assert_eq!(next.source_root,source);assert_eq!(flat.record_data().depth,2);
    let state=f.store.retention().unwrap();assert!(flat.matches_record_data(&state,&next.runnable_pin));
    assert!(overlay.acknowledgment().unwrap().matches_record_data(&state,"component-accepted-overlay"));
    assert!(prepared.runnable_dependency().unwrap().matches_record_data(&state,&prepared.runnable_pin));
    assert_eq!(state.pins.get(&prepared.seed_pin),Some(&prepared.seed_root));assert!(!f.store.object_path(&f.payload).unwrap().exists());
    let (_,child)=changed_source_candidate(&f,&next,b"new private child of flattened successor\n");let child=publish_component_overlay(&f,&next,child,"component-next-sibling");
    assert!(Arc::ptr_eq(child.acknowledgment().unwrap().source_parent(),next_parent));assert_eq!(child.acknowledgment().unwrap().record_data().depth,3);
}


#[test]#[ignore="strict Root-owned baked DATA + accepted-overlay full-pin cancellation window"]
fn cancelled_after_full_successor_pin_keeps_original_carrier_and_refuses_prepared_or_retry(){
    use jj_lib::{backend::CommitId,op_store::RefTarget,ref_name::{RefName,WorkspaceName},repo::Repo};
    use rudder_vcs_bridge::{initial_source::AcceptedSourceGeneration,projection::{project_source,verify_versioned_source},projection_io::ProjectionBudget,publication::INTEGRATION_BOOKMARK};
    let f=Fixture::new();let initial=budget();let initial_cancel=Arc::new(AtomicBool::new(false));let (_,mut context)=source_parent_context(&f,&initial,&initial_cancel);
    let prepared=f.run(&mut context,&initial,initial_cancel,||Ok(())).unwrap();
    let (source,runnable)=changed_source_candidate(&f,&prepared,b"accepted overlay awaiting successor parent\n");
    let overlay=publish_component_overlay(&f,&prepared,runnable.clone(),"component-window-overlay");let overlay_owner=overlay.acknowledgment().unwrap().clone();
    let repo=prepared.repository();let previous=repo.store().get_commit(&CommitId::new(hex::decode(&prepared.baseline.commit.0).unwrap())).unwrap();
    let source_value=decode_source(&f.store.read_object(&source).unwrap()).unwrap();let stage=f.store.begin_staging(Duration::from_secs(120)).unwrap();
    let projected=pollster::block_on(project_source(f.store.clone(),&source_value.directory,&previous.tree(),&source_value.projection_policy,None,None,ProjectionBudget::default())).unwrap();
    let verified=Arc::new(pollster::block_on(verify_versioned_source(f.store.clone(),&stage.id,&source,&source_value.projection_policy,&projected.tree,Some(&projected.record),ProjectionBudget::default())).unwrap());
    let mut tx=repo.start_transaction();let commit=pollster::block_on(tx.repo_mut().new_commit(vec![previous.id().clone()],projected.tree).write()).unwrap();
    tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(),commit.id().clone()).unwrap();
    tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK),RefTarget::normal(commit.id().clone()));
    let accepted=pollster::block_on(tx.commit("accepted overlay full-pin cancellation window")).unwrap();
    let original=budget();let cancel=Arc::new(AtomicBool::new(false));let observed=AtomicBool::new(false);let mut successor=None;
    let result=AcceptedSourceGeneration::prepare_overlay_retaining(&mut successor,f.store.clone(),accepted.clone(),verified.clone(),prepared.seed_cache().clone(),source.clone(),overlay_owner.clone(),
        prepared.baseline.clone(),prepared.seed_root.clone(),"component-window-full".into(),"component-window-seed".into(),original.clone(),cancel.clone(),||{
            let state=f.store.retention()?;
            if state.environment_dependencies.as_ref().is_some_and(|data|data.pins.contains_key("component-window-full")){
                observed.store(true,Ordering::Release);cancel.store(true,Ordering::Release);original.cancel();
                anyhow::bail!("cancel original successor immediately after actual full pin ACK");
            }Ok(())
        });
    assert!(result.is_err());assert!(observed.load(Ordering::Acquire));assert!(cancel.load(Ordering::Acquire));assert!(original.remaining().is_err());assert!(successor.is_some());
    // No process-loss/drop is needed: the whole original successor still owns
    // its acknowledged full publication and overlay through this late failure.
    let before=f.store.retention().unwrap();let full=before.environment_dependencies.as_ref().unwrap().pins.get("component-window-full").unwrap();
    assert_eq!(full.root,runnable);assert_eq!(full.depth,2);assert!(overlay_owner.matches_record_data(&before,"component-window-overlay"));
    assert!(prepared.runnable_dependency().unwrap().matches_record_data(&before,&prepared.runnable_pin));assert_eq!(before.pins.get(&prepared.seed_pin),Some(&prepared.seed_root));assert!(!before.pins.contains_key("component-window-seed"));
    let mut transferred=None;assert!(successor.as_mut().unwrap().take_prepared_retaining(&mut transferred).is_err());assert!(transferred.is_none());
    // Reusing the original occupied carrier cannot reopen the attempt. Neither
    // transfer nor replay receives a replacement clock or cancellation owner.
    assert!(AcceptedSourceGeneration::prepare_overlay_retaining(&mut successor,f.store.clone(),accepted,verified,prepared.seed_cache().clone(),source,overlay_owner,
        prepared.baseline.clone(),prepared.seed_root.clone(),"component-window-full".into(),"component-window-seed".into(),original.clone(),cancel.clone(),||Ok(())).is_err());
    assert!(successor.is_some());assert_eq!(f.store.retention().unwrap(),before);assert!(original.remaining().is_err());assert!(!f.store.object_path(&f.payload).unwrap().exists());
}

#[test]#[ignore="strict Root-owned baked DATA; original dependent failure diagnostics preserve carrier"]
fn dependent_initial_failure_diagnostics_preserve_original_error_and_active_stage(){
    let f=Fixture::new();let original=budget();let cancel=Arc::new(AtomicBool::new(false));
    let (_environment,mut context)=source_parent_context(&f,&original,&cancel);
    let conflict=f.store.put_blob(b"existing complete-local pin").unwrap();f.store.pin("component-dependent-runnable",&conflict).unwrap();
    let error=f.run(&mut context,&original,cancel,||Ok(())).err().expect("conflicting original pin must fail");
    assert!(error.to_string().contains("dependent publication failed"));
    let failure=context.dependent_publication_failure().expect("original cause retained");
    assert_eq!(failure.to_string(),"dependent root conflicts with existing complete-local/backed pin");
    let data=context.dependent_publication_diagnostic_data().unwrap();
    assert_eq!(data["lastStage"],"validate_state");assert_eq!(data["failure"]["present"],true);
    assert_eq!(data["failure"]["class"],"validation_or_budget");assert!(data["failure"]["ioRawOsError"].is_null());
    assert_eq!(data["acknowledged"],false);assert_eq!(data["gcLocked"],true);
    assert_eq!(data["actualLocalHashReads"],0);assert_eq!(data["actualLocalObjectSyncs"],0);
    assert_eq!(data["actualIoComplete"],false);assert_eq!(data["recordConstructed"],false);
    assert!(context.dependent_runnable_owner().is_err());assert!(context.source_parent().is_err());assert!(!f.seed.exists());
    assert!(std::ptr::eq(failure,context.dependent_publication_failure().unwrap()));
    // No retention readback while this unknown publication retains its GC lock.
}
