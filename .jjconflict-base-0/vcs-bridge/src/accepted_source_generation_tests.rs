//! Actual local jj/CAS byte-builder tests. No Root/PG/helper/Native acceptance
//! capability or kernel-readonly witness is fabricated by these fixtures.
use super::*;
use jj_lib::{config::{StackedConfig,ConfigLayer,ConfigSource},repo::{RepoLoader,StoreFactories}};
use rudder_engine::{snapshot::EnvironmentV2,store::{DirectoryEntry,DirectoryManifest,EntryNode}};
use crate::{JjTreeIdentity,projection::project_source};
use std::{collections::BTreeMap,os::unix::fs::DirBuilderExt,path::PathBuf};

struct Fixture {root:PathBuf,path:PathBuf,store:LocalStore,settings:UserSettings,repo:Arc<ReadonlyRepo>,
    environment:ObjectRef,policy:ObjectRef,profile:ObjectRef,topology:ObjectRef,previous_seed:ObjectRef,
    cache:Arc<RetainedSeedCasCache>}
impl Fixture {
    fn new()->Self {
        let root=std::env::temp_dir().join(format!("rudder-accepted-generation-{}",uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();let root=root.canonicalize().unwrap();
        let path=root.join("integration");std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let mut c=StackedConfig::with_defaults();let mut l=ConfigLayer::empty(ConfigSource::User);
        l.set_value("user.name","Generation fixture").unwrap();l.set_value("user.email","fixture@invalid.example").unwrap();c.add_layer(l);
        let settings=UserSettings::from_config(c).unwrap();let (_workspace,repo)=pollster::block_on(Workspace::init_internal_git(&settings,&path)).unwrap();
        let base=repo.store().get_commit(repo.view().get_wc_commit_id(WorkspaceName::DEFAULT).unwrap()).unwrap();
        let mut tx=repo.start_transaction();tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK),RefTarget::normal(base.id().clone()));
        let repo=pollster::block_on(tx.commit("initial actual source fixture")).unwrap();
        let git=repo.store().backend_impl::<GitBackend>().unwrap().git_repo_path();std::fs::write(git.join("HEAD"),format!("{}\n",base.id().hex())).unwrap();
        let store=LocalStore::open(root.join("cas")).unwrap();let empty=store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![]}).unwrap();
        let environment=store.put_metadata(ObjectKind::EnvironmentRoot,&EnvironmentV2{format_version:2,directory:empty,platform:BTreeMap::from([("fixture_abi".into(),"actual-local-jj".into())]),installer_inputs:vec![],provenance:BTreeMap::new()}).unwrap();store.pin("environment",&environment).unwrap();
        let profile_value=WorkspaceProfile::source_only();let policy=store.put_blob(&canonical_json(&SourceProjectionPolicy::from_import_policy(&profile_value.source_exclusions()).unwrap()).unwrap()).unwrap();
        let profile=store.put_blob(&canonical_json(&profile_value).unwrap()).unwrap();let topology=store.put_blob(&canonical_json(&HardlinkTopology{format_version:1,groups:vec![]}).unwrap()).unwrap();
        let seed=root.join("initial-seed");prepare_internal_metadata_template(&path,&seed,CommitIdentity::from_jj(&base),JjOperationId(repo.op_id().hex())).unwrap();
        let previous_seed=store.import_tree(seed,CheckpointConsistency::Quiesced).unwrap();store.pin("original-seed",&previous_seed).unwrap();
        let cache=RetainedSeedCasCache::capture(&path,store.clone()).unwrap();
        Self{root,path,store,settings,repo,environment,policy,profile,topology,previous_seed,cache}
    }
    fn accept(&mut self,bytes:&[u8])->(CommitIdentity,Arc<VerifiedSourceProjection>,ObjectRef,ObjectRef) {
        let baseline=self.repo.store().get_commit(self.repo.view().get_local_bookmark(RefName::new(INTEGRATION_BOOKMARK)).as_normal().unwrap()).unwrap();
        let previous=CommitIdentity::from_jj(&baseline);let blob=self.store.put_blob(bytes).unwrap();
        let directory=self.store.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:vec![DirectoryEntry::new(b"file",EntryNode::File{object:blob,mode:0o644}).unwrap()]}).unwrap();
        let stage=self.store.begin_staging(Duration::from_secs(120)).unwrap();
        let source=self.store.put_metadata(ObjectKind::SourceRoot,&SourceV2{format_version:2,consistency:CheckpointConsistency::Quiesced,directory:directory.clone(),projection_policy:self.policy.clone()}).unwrap();
        let projected=pollster::block_on(project_source(self.store.clone(),&directory,&baseline.tree(),&self.policy,None,None,ProjectionBudget::default())).unwrap();
        let verified=Arc::new(pollster::block_on(verify_versioned_source(self.store.clone(),&stage.id,&source,&self.policy,&projected.tree,Some(&projected.record),ProjectionBudget::default())).unwrap());
        let runnable=self.store.put_metadata(ObjectKind::RunnableRoot,&RunnableRoot{format_version:1,consistency:CheckpointConsistency::Quiesced,source:source.clone(),environment:Some(self.environment.clone()),view_root:directory,profile:self.profile.clone(),hardlinks:self.topology.clone(),knowledge_cursor:0}).unwrap();
        self.store.publish_extending(&stage.id,&format!("candidate-{}",uuid::Uuid::new_v4()),&runnable,&[self.environment.clone()]).unwrap();
        let mut tx=self.repo.start_transaction();let commit=pollster::block_on(tx.repo_mut().new_commit(vec![baseline.id().clone()],projected.tree).write()).unwrap();
        tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(),commit.id().clone()).unwrap();
        tx.repo_mut().set_local_bookmark_target(RefName::new(INTEGRATION_BOOKMARK),RefTarget::normal(commit.id().clone()));
        self.repo=pollster::block_on(tx.commit("actual accepted byte-builder operation")).unwrap();
        assert_eq!(JjTreeIdentity::from_jj(&commit.tree()),verified.receipt().tree);(previous,verified,source,runnable)
    }
    fn build(&self,previous:CommitIdentity,verified:Arc<VerifiedSourceProjection>,source:ObjectRef,runnable:ObjectRef,original:FinalizationBudget,cancel:Arc<AtomicBool>)->Result<AcceptedSourceGeneration>{
        let id=uuid::Uuid::new_v4();let mut out=None;let result=AcceptedSourceGeneration::prepare_retaining(&mut out,self.store.clone(),self.repo.clone(),verified,self.cache.clone(),source,runnable,self.environment.clone(),previous,self.previous_seed.clone(),format!("runnable-{id}"),format!("seed-{id}"),original,cancel,||Ok(()));
        result?;Ok(out.unwrap())
    }
}
impl Drop for Fixture{fn drop(&mut self){let _=std::fs::remove_dir_all(&self.root);}}
fn budget()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}

#[test]
fn actual_accepted_generations_preserve_parent_history_and_reuse_unchanged_object_versions(){
    let mut f=Fixture::new();let original_environment=f.environment.clone();let original_seed=f.previous_seed.clone();
    let (previous,verified,source,runnable)=f.accept(b"first actual edit\n");let old_head=previous.commit.clone();
    let mut first=f.build(previous,verified,source,runnable,budget(),Arc::new(AtomicBool::new(false))).unwrap();let mut first_prepared=None;first.take_prepared_retaining(&mut first_prepared).unwrap();let first_prepared=first_prepared.unwrap();
    assert!(first.seed_work().unwrap().read_files>0);assert_eq!(first_prepared.verified.receipt().source,first_prepared.source_root);
    f.previous_seed=first_prepared.seed_root.clone();
    let (previous,verified,source,runnable)=f.accept(b"second actual edit\n");let parent=previous.commit.clone();
    let mut second=f.build(previous,verified,source,runnable,budget(),Arc::new(AtomicBool::new(false))).unwrap();let work=second.seed_work().unwrap();assert!(work.reused_files>0);
    let mut out=None;second.take_prepared_retaining(&mut out).unwrap();let next=out.unwrap();
    assert_eq!(rudder_engine::snapshot::decode_runnable(&f.store.read_object(&next.runnable_root).unwrap()).unwrap().environment,Some(original_environment.clone()));
    assert!(f.store.retention().unwrap().pins.values().any(|r|r==&original_seed));assert!(f.store.retention().unwrap().pins.values().any(|r|r==&first_prepared.seed_root));
    let materialized=f.root.join("standalone-seed");f.store.export_tree(&next.seed_root,&materialized).unwrap();
    let private=f.root.join("standalone-private");std::fs::rename(materialized.join("metadata"),&private).unwrap();std::fs::rename(materialized.join("objects"),private.join(".git/objects")).unwrap();
    std::fs::rename(&f.path,f.root.join("retained-original-integration")).unwrap();
    let loader=RepoLoader::init_from_file_system(&f.settings,&private.join(".jj/repo"),&StoreFactories::default()).unwrap();
    let heads=pollster::block_on(loader.op_heads_store().get_op_heads()).unwrap();assert_eq!(heads.len(),1);assert_eq!(heads[0].hex(),next.operation.0);
    let op=pollster::block_on(loader.load_operation(&heads[0])).unwrap();let repo=pollster::block_on(loader.load_at(&op)).unwrap();
    let commit=repo.store().get_commit(&CommitId::new(hex::decode(&next.baseline.commit.0).unwrap())).unwrap();
    assert_eq!(commit.parent_ids(),&[CommitId::new(hex::decode(&parent.0).unwrap())]);
    let grandparent=repo.store().get_commit(&CommitId::new(hex::decode(&old_head.0).unwrap())).unwrap();assert!(repo.index().is_ancestor(grandparent.id(),commit.id()).unwrap());
    assert_eq!(std::fs::read_to_string(private.join(".git/HEAD")).unwrap(),format!("{}\n",next.baseline.commit.0));
    assert!(!private.join(".git/objects/info/alternates").exists());
}

#[test]
fn cancellation_keeps_old_task_pins_and_never_promotes_an_unacknowledged_successor(){
    let mut f=Fixture::new();let old=f.previous_seed.clone();let (previous,verified,source,runnable)=f.accept(b"candidate\n");let original=budget();original.cancel();
    assert!(f.build(previous,verified,source,runnable,original,Arc::new(AtomicBool::new(false))).is_err());
    assert!(f.store.retention().unwrap().pins.values().any(|r|r==&old));
}

#[test]
fn changed_operation_heads_or_nonchild_baseline_refuse_seed_generation(){
    let mut f=Fixture::new();let (mut previous,verified,source,runnable)=f.accept(b"candidate\n");
    previous.commit=GitCommitId("f".repeat(40));assert!(f.build(previous,verified,source,runnable,budget(),Arc::new(AtomicBool::new(false))).is_err());
    let mut f=Fixture::new();let (previous,verified,source,runnable)=f.accept(b"candidate\n");
    std::fs::write(f.path.join(".jj/repo/op_heads/heads").join("f".repeat(128)),b"").unwrap();
    assert!(f.build(previous,verified,source,runnable,budget(),Arc::new(AtomicBool::new(false))).is_err());
}

#[test]
fn mutation_after_seed_ack_retains_real_pins_but_refuses_final_prepared_transfer(){
    let mut f=Fixture::new();let (previous,verified,source,runnable)=f.accept(b"candidate\n");
    let mut built=f.build(previous,verified,source,runnable,budget(),Arc::new(AtomicBool::new(false))).unwrap();let seed=built.seed.as_ref().unwrap().root().unwrap().clone();
    std::fs::write(f.path.join(".jj/repo/store/type"),b"unexpected mutation").unwrap();
    let mut out=None;assert!(built.take_prepared_retaining(&mut out).is_err());assert!(out.is_none());assert!(f.store.retention().unwrap().pins.values().any(|r|r==&seed));
}

#[test]
fn original_cutoff_after_real_pin_keeps_partial_owner_and_never_retries_snapshot(){
    let mut f=Fixture::new();let (previous,verified,source,runnable)=f.accept(b"candidate\n");let original=budget();let actual_clock=original.clone();
    let pin="accepted-cutoff-runnable".to_owned();let observed_pin=pin.clone();let actual_store=f.store.clone();let old=f.previous_seed.clone();let mut owner=None;
    let result=AcceptedSourceGeneration::prepare_retaining(&mut owner,f.store.clone(),f.repo.clone(),verified,f.cache.clone(),source,runnable.clone(),f.environment.clone(),previous,old.clone(),pin.clone(),"accepted-cutoff-seed".into(),original,Arc::new(AtomicBool::new(false)),||{
        if actual_store.retention()?.pins.contains_key(&observed_pin){actual_clock.cancel();}actual_clock.remaining()?;Ok(())
    });
    assert!(result.is_err());let owner=owner.as_ref().unwrap();assert!(owner.pin_attempted&&owner.pin_ack&&!owner.acknowledged&&owner.prepared.is_none());
    assert_eq!(f.store.retention().unwrap().pins.get(&pin),Some(&runnable));assert!(f.store.retention().unwrap().pins.values().any(|r|r==&old));
    assert!(owner.seed.is_none());assert!(owner.time().is_err());
    // The same owner remains available to explicit recovery; no new budget is
    // accepted by its prepare/take methods and no replacement owner is minted.
}
#[test]
fn actual_deep_metadata_namespace_refuses_the_root_small_profile_before_seed_dispatch(){
    let mut f=Fixture::new();let (previous,verified,source,runnable)=f.accept(b"candidate\n");let old=f.previous_seed.clone();let mut path=f.path.join(".jj");
    for _ in 0..33{path=path.join("deep");std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();}
    assert!(f.build(previous,verified,source,runnable,budget(),Arc::new(AtomicBool::new(false))).is_err());assert!(f.store.retention().unwrap().pins.values().any(|r|r==&old));
}
