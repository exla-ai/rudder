//! Existing real backed preparation fixture; no Root/SDK execution authority.
use super::*;
use crate::{snapshot::{SourceV2,RunnableRoot},protocol::CheckpointConsistency,workspace::{WorkspaceProfile,retained_build_input::RetainedRunnableBuildInput},
    store::environment_dependencies::{RetainedEnvironmentSubgraph,EnvironmentDependentPinOwner,RetainedRunnableInputs}};
fn environment(f:&Fixture)->(RetainedBackedPreparation,Arc<RetainedEnvironmentSubgraph>){
    fs::set_permissions(&f.index.locks,fs::Permissions::from_mode(0o700)).unwrap();let mut preparation=None;f.run(&mut preparation,None).unwrap();let preparation=preparation.unwrap();
    let mut environment=None;preparation.derive_environment_subgraph_retaining(&mut environment,Clock::new()).unwrap();(preparation,environment.unwrap())
}
fn publish(f:&Fixture,environment:Arc<RetainedEnvironmentSubgraph>,prepared_partition:bool)->(Arc<EnvironmentDependentPinOwner>,ObjectRef,ObjectRef){
    let store=&f.index.store;let stage=store.begin_staging(Duration::from_secs(120)).unwrap();let blob=store.put_blob(b"pub fn component() {}\n").unwrap();
    let writer=LocalDirectoryStore::new(store,&stage.id);let directory=PagedDirectory::new(&writer).build(0o755,vec![DirectoryEntry::new(b"lib.rs",EntryNode::File{object:blob.clone(),logical_size:blob.size_bytes,mode:0o644}).unwrap()]).unwrap();
    let mut profile=WorkspaceProfile::source_only();if prepared_partition{profile.prepared=vec![vec![b"target".to_vec()]];}
    let excluded_paths_hex:Vec<Vec<String>>=profile.source_exclusions().excluded_paths.iter().map(|path|path.iter().map(hex::encode).collect()).collect();
    let policy=store.put_blob(&canonical_json(&serde_json::json!({"format_version":1,"excluded_paths_hex":excluded_paths_hex})).unwrap()).unwrap();
    let source=store.put_metadata(ObjectKind::SourceRoot,&SourceV2{format_version:2,consistency:CheckpointConsistency::Quiesced,directory:directory.clone(),projection_policy:policy.clone()}).unwrap();
    let profile=store.put_blob(&canonical_json(&profile).unwrap()).unwrap();let topology=store.put_blob(b"{\"format_version\":1,\"groups\":[]}").unwrap();
    let root=store.put_metadata(ObjectKind::RunnableRoot,&RunnableRoot{format_version:1,consistency:CheckpointConsistency::Quiesced,source,environment:Some(environment.root_data().unwrap().clone()),view_root:directory,profile:profile.clone(),hardlinks:topology,knowledge_cursor:0}).unwrap();
    let mut publication=None;store.publish_environment_dependent_retaining(&mut publication,stage.id,"canonical-input".into(),root,environment,Clock::new()).unwrap();
    (publication.unwrap().acknowledgment().unwrap().clone(),policy,profile)
}
#[test]fn canonical_build_input_uses_same_real_dependent_owner_without_foreign_pin_or_payload_copy(){let f=Fixture::new();let(_preparation,env)=environment(&f);let(owner,policy,_)=publish(&f,env,false);let before=f.state();let mut input=None;
    RetainedRunnableBuildInput::capture_retaining(&mut input,f.index.store.clone(),RetainedRunnableInputs::Environment(owner.clone()),policy,Default::default(),Clock::new()).unwrap();
    let input=input.as_ref().unwrap().input().unwrap();assert_eq!(input.checkpoint(),owner.root_data());assert_eq!(input.base_environment(),Some(owner.environment().root_data().unwrap()));assert_eq!(input.hardlink_group_count(),0);
    assert_eq!(f.state(),before);assert!(!f.index.store.object_path(&f.payload).unwrap().exists());assert!(f.index.store.pin("ordinary-canonical",owner.root_data()).is_err());
}
#[test]fn canonical_build_input_rejects_wrong_policy_and_unimplemented_prepared_partition_with_original_owner_retained(){for fault in 0..2{let f=Fixture::new();let(_prep,env)=environment(&f);let(owner,mut policy,_)=publish(&f,env,fault==1);
    if fault==0{policy=f.index.store.put_blob(b"wrong actual expected policy").unwrap();}let mut input=None;
    assert!(RetainedRunnableBuildInput::capture_retaining(&mut input,f.index.store.clone(),RetainedRunnableInputs::Environment(owner),policy,Default::default(),Clock::new()).is_err());assert!(input.as_ref().unwrap().input().is_err());assert!(input.as_ref().unwrap().failure().is_some());}}
#[test]fn canonical_build_input_cancelled_before_io_keeps_original_carrier(){let f=Fixture::new();let(_prep,env)=environment(&f);let(owner,policy,_)=publish(&f,env,false);let original=Clock::new();original.0.cancel();let mut input=None;
    assert!(RetainedRunnableBuildInput::capture_retaining(&mut input,f.index.store.clone(),RetainedRunnableInputs::Environment(owner),policy,Default::default(),original).is_err());assert!(input.is_some());assert!(input.unwrap().input().is_err());}
#[test]fn canonical_build_input_hashes_local_metadata_and_keeps_failed_read_on_corruption(){let f=Fixture::new();let(_prep,env)=environment(&f);let(owner,policy,profile)=publish(&f,env,false);
    let path=f.index.store.object_path(&profile).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'?';profile.size_bytes as usize]).unwrap();let mut input=None;
    assert!(RetainedRunnableBuildInput::capture_retaining(&mut input,f.index.store.clone(),RetainedRunnableInputs::Environment(owner),policy,Default::default(),Clock::new()).is_err());assert!(input.as_ref().unwrap().input().is_err());assert!(input.as_ref().unwrap().failure().is_some());}
