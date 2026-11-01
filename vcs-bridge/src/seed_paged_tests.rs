//! Actual local jj/Git/CAS/CoW tests. No Root/PG/EngineOwnership capability,
//! kernel mount receipt or accepted-source grant is fabricated here.
use super::*;
use jj_lib::{config::{StackedConfig,ConfigLayer,ConfigSource},settings::UserSettings,workspace::Workspace,
    repo::Repo,ref_name::{RefName,WorkspaceName},op_store::RefTarget,git_backend::GitBackend,object_id::ObjectId as _};
use rudder_engine::{store::{DirectoryManifest,DirectoryEntry as LegacyEntry,EntryNode as LegacyNode},
    vfs::{VfsManager,VfsView},protocol::OwnerEpoch};
use crate::{projection::{project_source,ProjectionRecord},projection_io::ProjectionBudget,vfs_provider::SourceProjectionPolicy,
    publication::INTEGRATION_BOOKMARK,working_copy};
use std::{os::unix::fs::{DirBuilderExt,MetadataExt},time::Duration};
struct Fixture {root:PathBuf,path:PathBuf,store:LocalStore,repo:Arc<jj_lib::repo::ReadonlyRepo>,settings:UserSettings,cache:Arc<RetainedSeedCasCache>,projection:Option<ProjectionRecord>}
impl Fixture {
    fn new(files:usize,large:usize)->Self {
        let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};
        let root=base.join(format!("rudder-seed-v2-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let root=root.canonicalize().unwrap();let path=root.join("integration");fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let mut config=StackedConfig::with_defaults();let mut layer=ConfigLayer::empty(ConfigSource::User);
        layer.set_value("user.name","V2 actual fixture").unwrap();layer.set_value("user.email","v2@invalid.example").unwrap();config.add_layer(layer);
        let settings=UserSettings::from_config(config).unwrap();let(_workspace,repo)=pollster::block_on(Workspace::init_internal_git(&settings,&path)).unwrap();
        let store=LocalStore::open(root.join("cas")).unwrap();let cache=RetainedSeedCasCache::capture_profile(&path,store.clone(),SeedProfile::PagedCowV2).unwrap();
        let mut f=Self{root,path,store,repo,settings,cache,projection:None};f.accept(files,large);f
    }
    fn accept(&mut self,files:usize,large:usize)->crate::projection_io::ProjectionWork{
        let parent=self.repo.store().get_commit(self.repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();
        let mut entries=Vec::new();for i in 0..files {let object=self.store.put_blob(format!("actual Git blob {i}\n").as_bytes()).unwrap();
            entries.push(LegacyEntry::new(format!("file-{i:06}").as_bytes(),LegacyNode::File{object,mode:0o644}).unwrap());}
        if large>0 {let mut random=0xd2f4_9957_2ba0_006du64;let mut bytes=vec![0;large];for b in &mut bytes{random^=random<<13;random^=random>>7;random^=random<<17;*b=random as u8;}
            let object=self.store.put_blob(&bytes).unwrap();entries.push(LegacyEntry::new(b"large",LegacyNode::File{object,mode:0o644}).unwrap());}
        let root=self.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries}).unwrap();
        let policy=self.store.put_blob(&canonical_json(&SourceProjectionPolicy::from_import_policy(&rudder_engine::workspace::WorkspaceProfile::source_only().source_exclusions()).unwrap()).unwrap()).unwrap();
        let cold=self.projection.is_none();
        let projected=pollster::block_on(project_source(self.store.clone(),&root,&parent.tree(),&policy,None,self.projection.as_ref(),ProjectionBudget::default())).unwrap();
        if cold{assert_eq!(projected.work.reused_leaf_values,0,"cold None must not infer mapping equality");}
        let mut tx=self.repo.start_transaction();let commit=pollster::block_on(tx.repo_mut().new_commit(vec![parent.id().clone()],projected.tree).write()).unwrap();
        tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(),commit.id().clone()).unwrap();
        tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK),RefTarget::normal(commit.id().clone()));self.repo=pollster::block_on(tx.commit("V2 fixture accepted history")).unwrap();
        let git=self.repo.store().backend_impl::<GitBackend>().unwrap().git_repo_path();fs::write(git.join("HEAD"),format!("{}\n",commit.id().hex())).unwrap();
        // ONLY the actual returned record from this exact accepted tree/policy.
        // The first import remains a genuine cold None path.
        self.projection=Some(projected.record);projected.work
    }
    fn blob_stamps(&self)->BTreeMap<String,(PathBuf,LooseBlobStamp)>{
        let commit=self.repo.store().get_commit(self.repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();
        let git=self.repo.store().backend_impl::<GitBackend>().unwrap().git_repo_path();let mut stamps=BTreeMap::new();
        for(path,value)in commit.tree().entries(){
            let name=path.as_internal_file_string();if !name.starts_with("file-")&&name!="large"{continue;}
            let value=value.unwrap();let Some(Some(jj_lib::backend::TreeValue::File{id,..}))=value.as_resolved()else{panic!("actual fixture entry is not a resolved Git blob")};
            let oid=id.hex();assert_eq!(oid.len(),40);let file=git.join("objects").join(&oid[..2]).join(&oid[2..]);
            assert!(stamps.insert(name.into(),(file.clone(),loose_blob_stamp(&file))).is_none());
        }stamps
    }
    fn seed(&self,previous:Option<&ObjectRef>)->PagedSeedGeneration{
        let commit=self.repo.store().get_commit(self.repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();let mut out=None;
        PagedSeedGeneration::prepare_retaining(&mut out,self.cache.clone(),CommitIdentity::from_jj(&commit),JjOperationId(self.repo.op_id().hex()),
            format!("seed-v2:{}",uuid::Uuid::new_v4()),previous,budget(),Arc::new(AtomicBool::new(false)),&mut||Ok(())).unwrap();out.unwrap()
    }
}
// Match every field used by the producer's file-version cache. The object
// pathname comes from the ACTUAL accepted jj tree's FileId, never a guessed ID.
#[derive(Clone,Debug,PartialEq,Eq)]
struct LooseBlobStamp{dev:u64,ino:u64,len:u64,mode:u32,mtime:(i64,i64),ctime:(i64,i64)}
fn loose_blob_stamp(path:&Path)->LooseBlobStamp{
    let m=fs::symlink_metadata(path).unwrap();assert!(m.is_file()&&!m.file_type().is_symlink());
    assert_eq!(m.uid(),unsafe{libc::geteuid()});assert_eq!(m.mode()&0o022,0);
    LooseBlobStamp{dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())}
}
impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_dir_all(&self.root);}}
fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn lookup(view:&VfsView,path:&[&[u8]])->u64{let mut id=1;for p in path{id=view.lookup(id,p).unwrap().ino;}id}

#[test]
fn actual_large_git_object_uses_filenodes_and_unchanged_successor_reuses_content(){
    let f=Fixture::new(24,9<<20);let first=f.seed(None);let root=first.root().unwrap().clone();
    let(d,_,manifest)=read_paged_seed(&f.store,&root,||Ok(())).unwrap();assert!(d.object_bytes>8<<20);
    let big=manifest.objects.iter().find(|(_,f)|f.size_bytes>4<<20).unwrap();let object=lookup_path(&f.store,&d.objects,big.0).unwrap().unwrap();
    assert!(matches!(object.node,EntryNode::File{object,..}if object.kind==ObjectKind::File));
    let warm=f.seed(Some(&root));assert_eq!(warm.root().unwrap(),&root);assert_eq!(warm.work().read_files,0);assert!(warm.work().reused_files>0);
    assert!(f.store.retention().unwrap().pins.values().filter(|r|*r==&root).count()>=2);
    // V1 refuses honestly. It is never a truncating fallback for V2.
    let small=RetainedSeedCasCache::capture(&f.path,f.store.clone()).unwrap();let mut rejected=None;
    assert!(SeedCasGeneration::prepare_retaining(&mut rejected,small,d.baseline,d.operation,format!("v1-refusal:{}",uuid::Uuid::new_v4()),&root,budget(),Arc::new(AtomicBool::new(false)),&mut||Ok(())).is_err());
    assert!(rejected.is_some());assert!(rejected.as_ref().unwrap().root().is_err());
}

#[test]
#[ignore="large actual Git/jj + 4096 objects; run serial on /workspace"]
fn large_real_history_exceeds_old_file_profile_and_private_writes_are_isolated(){
    let mut f=Fixture::new(4096,10<<20);let before=f.seed(None);let old=before.root().unwrap().clone();
    let inherited=f.blob_stamps();assert_eq!(inherited.len(),4097);assert_eq!(inherited.keys().filter(|n|n.starts_with("file-")).count(),4096);
    let projected=f.accept(4100,10<<20);assert_eq!(projected.reused_leaf_values,4097,"actual prior record must cover inherited files and large blob");
    let after=f.blob_stamps();assert_eq!(after.len(),4101);
    for(name,expected)in &inherited{assert_eq!(after.get(name),Some(expected),"inherited Git object/stamps changed: {name}");}
    let next=f.seed(Some(&old));let root=next.root().unwrap();let(d,_,manifest)=read_paged_seed(&f.store,root,||Ok(())).unwrap();
    assert!(d.object_files>1024);assert!(next.work().reused_files>4096,"unchanged inherited inode/stamp witness, actual seed work: {:?}",next.work());assert!(manifest.metadata.len()+manifest.objects.len()>1024);
    let manager=VfsManager::open(f.root.join("logical-cow"),f.store.clone()).unwrap();manager.prepare_paged(&d.private).unwrap();
    let a=manager.fork_paged(&uuid::Uuid::new_v4().to_string(),&d.private,OwnerEpoch{owner:"fixture".into(),epoch:1}).unwrap();
    let b=manager.fork_paged(&uuid::Uuid::new_v4().to_string(),&d.private,OwnerEpoch{owner:"fixture".into(),epoch:1}).unwrap();
    let a_head=lookup(&a,&[b".git",b"HEAD"]);let b_head=lookup(&b,&[b".git",b"HEAD"]);let original=b.read(b_head,0,128).unwrap();
    let handle=a.open_handle(a_head,libc::O_RDWR,false).unwrap();a.write(a_head,0,b"PRIVATE-ONLY\n",true).unwrap();a.close_handle(a_head,handle).unwrap();
    assert_eq!(b.read(b_head,0,128).unwrap(),original);assert!(a.read(a_head,0,128).unwrap().starts_with(b"PRIVATE-ONLY\n"));
    let snapshot=a.seal().unwrap();assert_ne!(snapshot.directory,d.private);assert!(f.store.retention().unwrap().pins.values().any(|r|r==&old));
    assert_eq!(read_paged_seed(&f.store,root,||Ok(())).unwrap().0,d);
    // These logical-only APIs issue NO kernel/TLS/Root retirement proof.
}

#[test]
fn cancellation_and_refusal_keep_original_owner_without_publication_ack(){
    let f=Fixture::new(8,0);let first=f.seed(None);let old=first.root().unwrap().clone();
    let commit=f.repo.store().get_commit(f.repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();
    let original=budget();let cancelled=Arc::new(AtomicBool::new(true));let mut out=None;
    assert!(PagedSeedGeneration::prepare_retaining(&mut out,f.cache.clone(),CommitIdentity::from_jj(&commit),JjOperationId(f.repo.op_id().hex()),
        "cancelled-v2".into(),Some(&old),original,cancelled,&mut||Ok(())).is_err());
    assert!(out.is_some());assert!(out.as_ref().unwrap().root().is_err());assert!(f.store.retention().unwrap().pins.values().any(|r|r==&old));
    let mut out=None;let mut calls=0;
    assert!(PagedSeedGeneration::prepare_retaining(&mut out,f.cache.clone(),CommitIdentity::from_jj(&commit),JjOperationId(f.repo.op_id().hex()),
        "validation-v2".into(),Some(&old),budget(),Arc::new(AtomicBool::new(false)),&mut||{calls+=1;if calls==3{anyhow::bail!("injected lease refusal; no PG grant")};Ok(())}).is_err());
    assert!(out.as_ref().unwrap().root().is_err());assert!(f.store.retention().unwrap().pins.values().any(|r|r==&old));
}

#[cfg(target_os="linux")]
#[test]
fn rejected_unqualified_cow_factories_share_one_manifest_allocation(){
    let f=Fixture::new(512,0);let seed=f.seed(None);let root=seed.root().unwrap();let(d,id,manifest)=read_paged_seed(&f.store,root,||Ok(())).unwrap();
    // Explicit negative fixture: neither retained seed nor kernel RO proof.
    // Production CoW creation MUST refuse; no positive Root token is made.
    let template=VerifiedMetadataTemplate{root:f.root.clone(),object_mount:f.root.join("not-mounted"),manifest:Arc::new(manifest),identity:id,
        require_readonly:false,retained_seed:None,paged:Some(d.clone())};
    let policy=f.store.put_blob(b"fixture-policy-data").unwrap();let binding=AttemptBinding{format_version:1,project_id:"fixture-project".into(),owner:OwnerEpoch{owner:"fixture".into(),epoch:1},
        run_id:"fixture-run".into(),run_epoch:1,attempt_id:"fixture-attempt".into(),view_id:"fixture-view".into(),seed:root.clone(),baseline:d.baseline,baseline_operation:d.operation,projection_policy:policy.clone(),profile:policy};
    let mut a=None;let mut b=None;
    for out in [&mut a,&mut b]{assert!(fork_private_metadata_cow_retaining(out,&template,&f.root.join("not-a-root-mount"),&uuid::Uuid::new_v4().to_string(),&d.private,binding.clone(),None,&budget(),&AtomicBool::new(false)).is_err());}
    assert!(a.as_ref().unwrap().shares_manifest_allocation(&template));assert!(b.as_ref().unwrap().shares_manifest_allocation(&template));
    assert!(!f.root.join("not-a-root-mount").exists());
}

#[test]
fn actual_metadata_mutation_after_pin_ack_prevents_generation_readiness(){
    let f=Fixture::new(8,0);let first=f.seed(None);let old=first.root().unwrap().clone();
    let commit=f.repo.store().get_commit(f.repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();let mut out=None;let mut changed=false;
    let result=PagedSeedGeneration::prepare_retaining(&mut out,f.cache.clone(),CommitIdentity::from_jj(&commit),JjOperationId(f.repo.op_id().hex()),
        "mutation-after-pin".into(),Some(&old),budget(),Arc::new(AtomicBool::new(false)),&mut||{
            if !changed&&f.store.retention()?.pins.contains_key("mutation-after-pin"){
                fs::write(f.path.join(".jj/working_copy/type"),b"changed-after-original-capture")?;changed=true;
            }Ok(())
        });
    assert!(changed&&result.is_err());assert!(out.as_ref().unwrap().root().is_err());
    assert!(f.store.retention().unwrap().pins.values().any(|r|r==&old));assert!(f.store.retention().unwrap().pins.contains_key("mutation-after-pin"));
}


#[test]
fn actual_initial_import_selects_paged_producer_without_materializing_flat_seed(){
    let f=Fixture::new(16,0);let private=f.root.join("initial-private");fs::DirBuilder::new().mode(0o700).create(&private).unwrap();
    let seed=f.root.join("initial-unmaterialized-seed");
    let empty=f.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![]}).unwrap();
    let environment=f.store.put_metadata(ObjectKind::EnvironmentRoot,&rudder_engine::snapshot::EnvironmentV2{format_version:2,directory:empty,
        platform:BTreeMap::from([("fixture".into(),"inert-no-model".into())]),installer_inputs:vec![],provenance:BTreeMap::new()}).unwrap();
    f.store.pin("initial-fixture-environment",&environment).unwrap();
    let commit=f.repo.store().get_commit(f.repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();
    let expected=crate::GitCommitId(commit.id().hex());let mut retained=None;let original=budget();
    let prepared=pollster::block_on(crate::initial_source::prepare_initial_source_profile_retaining(f.store.clone(),crate::initial_source::InitialSourceRequest{
        source_git:&f.path.join(".jj/repo/store/git"),private_repository:&private,seed_destination:&seed,expected_head:&expected,environment:&environment,settings:&f.settings},
        SeedProfile::PagedCowV2,&mut retained,&original,Arc::new(AtomicBool::new(false)),||Ok(()))).unwrap();
    assert!(retained.as_ref().unwrap().root().is_ok());assert!(!seed.exists());
    let(d,_,_)=read_paged_seed(&f.store,&prepared.seed_root,||Ok(())).unwrap();assert_eq!(d.baseline,prepared.baseline);
    assert_eq!(prepared.seed_cache().profile(),SeedProfile::PagedCowV2);assert_eq!(d.operation,prepared.operation);
    assert_eq!(rudder_engine::snapshot::decode_runnable(&f.store.read_object(&prepared.runnable_root).unwrap()).unwrap().environment,Some(environment.clone()));
    let cancelled_private=f.root.join("never-initialized");fs::DirBuilder::new().mode(0o700).create(&cancelled_private).unwrap();let cancelled_seed=f.root.join("cancelled-inert-seed");
    let mut failed=None;let cancelled_budget=budget();
    assert!(pollster::block_on(crate::initial_source::prepare_initial_source_profile_retaining(f.store.clone(),crate::initial_source::InitialSourceRequest{
        source_git:&f.path.join(".jj/repo/store/git"),private_repository:&cancelled_private,seed_destination:&cancelled_seed,expected_head:&expected,environment:&environment,settings:&f.settings},
        SeedProfile::PagedCowV2,&mut failed,&cancelled_budget,Arc::new(AtomicBool::new(true)),||Ok(()))).is_err());
    assert!(!cancelled_private.join(".jj").exists()&&!cancelled_seed.exists());assert!(cancelled_budget.remaining().is_err());
}

#[cfg(target_os="linux")]
fn replace_file(store:&LocalStore,dirs:&PagedDirectory<'_,LocalDirectoryStore<'_>>,root:&ObjectRef,path:&[&[u8]],bytes:&[u8])->ObjectRef{
    let name=path[0];let node=if path.len()==1{let object=store.put_blob(bytes).unwrap();EntryNode::File{logical_size:bytes.len()as u64,object,mode:0o600}}
        else{let old=dirs.lookup(root,name).unwrap().map(|e|match e.node{EntryNode::Directory{object}=>object,_=>panic!("fixture path crosses file")})
            .unwrap_or_else(||dirs.build(0o700,Vec::<DirectoryEntry>::new()).unwrap());
            EntryNode::Directory{object:replace_file(store,dirs,&old,&path[1..],bytes)}};
    dirs.update(root,name,Some(node)).unwrap().root
}

#[cfg(target_os="linux")]
#[test]
fn merkle_archive_preserves_actual_parent_history_without_shared_path_or_pack_copy(){
    let mut f=Fixture::new(32,0);f.accept(34,0);let seed=f.seed(None);let source=seed.root().unwrap().clone();
    let(descriptor,_,manifest)=read_paged_seed(&f.store,&source,||Ok(())).unwrap();let shared=f.root.join("shared-export-data-only").join("objects");
    let original=budget();let cancel=Arc::new(AtomicBool::new(false));let mut floor=None;
    CowMetadataFloor::prepare_retaining(&mut floor,f.store.clone(),source.clone(),&shared,"test-live-floor".into(),&original,cancel.clone()).unwrap();
    let stage=f.store.begin_staging(Duration::from_secs(120)).unwrap();let adapter=LocalDirectoryStore::new(&f.store,&stage.id);let dirs=PagedDirectory::new(&adapter);
    let floor=floor.as_ref().unwrap().root().unwrap();let raw=replace_file(&f.store,&dirs,floor,&[b".git",b"private-note"],b"only this attempt\n");
    let localized=super::super::cow_archive::localize_private_tree(&f.store,&dirs,&descriptor.objects,&raw,&shared,&original,&cancel).unwrap();
    f.store.publish_extending(&stage.id,"test-independent-archive",&localized,&[source.clone(),floor.clone()]).unwrap();
    assert!(lookup_path(&f.store,&localized,".git/objects/info/alternates").unwrap().is_none());
    for(path,file)in &manifest.objects{let old=lookup_path(&f.store,&descriptor.objects,path).unwrap().unwrap();
        let new=lookup_path(&f.store,&localized,&format!(".git/objects/{path}")).unwrap().unwrap();
        assert_eq!(old.node,new.node,"inherited CAS object must remain identical: {path}");assert!(file.size_bytes<=V2_MAX_FILE_BYTES);}
    let hardlinks=f.store.put_blob(&canonical_json(&rudder_engine::tree::HardlinkTopology{format_version:1,groups:vec![]}).unwrap()).unwrap();
    let checkout=f.root.join("independent-history");rudder_engine::tree::export_quiesced(&f.store,&rudder_engine::tree::TreeSnapshot{format_version:1,directory:localized,hardlinks},
        &checkout,&rudder_engine::tree::ImportBudget::default()).unwrap();
    assert!(!shared.exists(),"the logical test never creates a Root mount");
    let git=gix::open(checkout.join(".git")).unwrap();assert_eq!(git.head().unwrap().id().unwrap().detach().to_string(),descriptor.baseline.commit.0);
    for parent in &descriptor.baseline.parents{assert!(git.find_object(gix::ObjectId::from_hex(parent.0.as_bytes()).unwrap()).is_ok());}
    let workspace=Workspace::load(&f.settings,&checkout,&jj_lib::repo::StoreFactories::default(),&jj_lib::workspace::default_working_copy_factories()).unwrap();
    let repository=pollster::block_on(workspace.repo_loader().load_at_head()).unwrap();assert_eq!(repository.op_id().hex(),descriptor.operation.0);
    assert_eq!(fs::read(checkout.join(".git/private-note")).unwrap(),b"only this attempt\n");
    assert!(f.store.retention().unwrap().pins.values().any(|r|r==&source));
    // DATA algorithm coverage only: no synthetic actor/TLS/Root ACK is issued.
}

#[cfg(target_os="linux")]
#[test]
fn merkle_archive_refuses_foreign_alternate_and_conflicting_object_name(){
    let f=Fixture::new(8,0);let seed=f.seed(None);let source=seed.root().unwrap().clone();let(d,_,m)=read_paged_seed(&f.store,&source,||Ok(())).unwrap();
    let shared=f.root.join("not-a-root-mount/objects");let original=budget();let cancel=Arc::new(AtomicBool::new(false));let mut floor=None;
    CowMetadataFloor::prepare_retaining(&mut floor,f.store.clone(),source.clone(),&shared,"test-refusal-floor".into(),&original,cancel.clone()).unwrap();
    let stage=f.store.begin_staging(Duration::from_secs(120)).unwrap();let adapter=LocalDirectoryStore::new(&f.store,&stage.id);let dirs=PagedDirectory::new(&adapter);
    let raw=floor.as_ref().unwrap().root().unwrap();let foreign=replace_file(&f.store,&dirs,raw,&[b".git",b"objects",b"info",b"alternates"],b"/foreign/objects\n");
    assert!(super::super::cow_archive::localize_private_tree(&f.store,&dirs,&d.objects,&foreign,&shared,&original,&cancel).is_err());
    let path=m.objects.keys().next().unwrap();let mut parts=vec![b".git".as_slice(),b"objects".as_slice()];parts.extend(path.split('/').map(str::as_bytes));
    let conflict=replace_file(&f.store,&dirs,raw,&parts,b"different bytes at original object name");
    assert!(super::super::cow_archive::localize_private_tree(&f.store,&dirs,&d.objects,&conflict,&shared,&original,&cancel).is_err());
    assert_eq!(f.store.retention().unwrap().pins.get("test-refusal-floor"),Some(raw));
}

// Explicit repository-sized producer component; never included in default runs.
#[cfg(target_os="linux")]
#[path="seed_real_repo_qualification.rs"]
mod real_repo_qualification;
