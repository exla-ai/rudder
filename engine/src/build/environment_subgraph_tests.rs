//! Actual CAS/SQLite/FD component cases. No Root, project or SDK owner is
//! fabricated, and none of these tests starts a process. Definitions UNRUN.
use super::*;
use std::os::unix::fs::DirBuilderExt;
use crate::{protocol::CheckpointConsistency,snapshot::{SourceV2,RunnableRoot},workspace::WorkspaceProfile,
    store::environment_dependencies::{RetainedEnvironmentSubgraph,EnvironmentDependentPinOwner,RetainedRunnableInputs}};
fn fixture()->Fixture{let f=Fixture::new();fs::set_permissions(&f.index.locks,fs::Permissions::from_mode(0o700)).unwrap();f}
fn prepared(f:&Fixture)->RetainedBackedPreparation{let mut out=None;f.run(&mut out,None).unwrap();out.unwrap()}
fn boundary(prep:&RetainedBackedPreparation)->Arc<RetainedEnvironmentSubgraph>{let mut out=None;prep.derive_environment_subgraph_retaining(&mut out,Clock::new()).unwrap();out.unwrap()}
fn runnable(f:&Fixture,environment:&ObjectRef,text:&[u8],stage:&str)->ObjectRef{
    let store=&f.index.store;let blob=store.put_blob(text).unwrap();
    let writer=LocalDirectoryStore::new(store,stage);let directory=PagedDirectory::new(&writer).build(0o755,vec![
        DirectoryEntry::new(b"data",EntryNode::File{object:blob.clone(),logical_size:blob.size_bytes,mode:0o644}).unwrap()]).unwrap();
    let profile=WorkspaceProfile::source_only();let excluded_paths_hex:Vec<Vec<String>>=profile.source_exclusions().excluded_paths.iter()
        .map(|path|path.iter().map(hex::encode).collect()).collect();
    let policy=store.put_blob(&canonical_json(&serde_json::json!({"format_version":1,"excluded_paths_hex":excluded_paths_hex})).unwrap()).unwrap();
    let source=store.put_metadata(ObjectKind::SourceRoot,&SourceV2{format_version:2,consistency:CheckpointConsistency::Quiesced,directory:directory.clone(),projection_policy:policy}).unwrap();
    let profile=store.put_blob(&canonical_json(&profile).unwrap()).unwrap();let hardlinks=store.put_blob(b"{\"format_version\":1,\"groups\":[]}").unwrap();
    store.put_metadata(ObjectKind::RunnableRoot,&RunnableRoot{format_version:1,consistency:CheckpointConsistency::Quiesced,source,environment:Some(environment.clone()),view_root:directory,profile,hardlinks,knowledge_cursor:0}).unwrap()
}
fn publish(f:&Fixture,environment:Arc<RetainedEnvironmentSubgraph>,name:&str,text:&[u8])->Arc<EnvironmentDependentPinOwner>{
    let stage=f.index.store.begin_staging(Duration::from_secs(120)).unwrap();let root=runnable(f,environment.root_data().unwrap(),text,&stage.id);let mut out=None;
    f.index.store.publish_environment_dependent_retaining(&mut out,stage.id,name.into(),root,environment,Clock::new()).unwrap();out.unwrap().acknowledgment().unwrap().clone()
}
fn read(owner:&Arc<EnvironmentDependentPinOwner>,r:ObjectRef)->Result<Vec<u8>>{let mut out=None;owner.read_retaining(&mut out,r,Clock::new())?;Ok(out.unwrap().verified_bytes()?.to_vec())}
fn rewrite_state(f:&Fixture,change:impl FnOnce(&mut RetentionState)){
    let mut state=f.state();change(&mut state);let checksum=ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&state).unwrap()).digest;
    fs::write(f.index.store.root().join("retention.json"),canonical_json(&serde_json::json!({"checksum":checksum,"state":state})).unwrap()).unwrap();
}
#[test]
fn genuine_completed_preparation_outlives_its_clock_without_reconstructing_ack(){
    let f=fixture();let prep=prepared(&f);prep.core.original.cancel();assert!(prep.core.check().is_err());
    let env=boundary(&prep);assert_eq!(env.root_data().unwrap(),&prep.acknowledgment().unwrap().prepared().root);
    let owner=publish(&f,env,"dep-clock",b"source");assert_eq!(read(&owner,f.payload.clone()).unwrap(),f.payload_bytes);
    let late=fixture();let mut unknown=late.staged(api::TestPoint::CancelAfterCommit);assert!(unknown.run_backed().is_err());assert_eq!(unknown.core.sql,SqlOutcome::Committed);
    let mut out=None;assert!(unknown.derive_environment_subgraph_retaining(&mut out,Clock::new()).is_err());assert!(out.is_none());
}
#[test]
fn sibling_and_successor_roots_share_one_exact_environment_not_runnable_ancestry(){
    let f=fixture();let prep=prepared(&f);let env=boundary(&prep);let a=publish(&f,env.clone(),"dep-a",b"old");let b=publish(&f,env.clone(),"dep-b",b"new");
    assert!(Arc::ptr_eq(a.environment(),b.environment()));assert_eq!(a.record_data().depth,2);assert_eq!(b.record_data().depth,2);
    assert!(a.record_data().local_bytes<64*1024&&b.record_data().local_bytes<64*1024);assert!(!f.index.store.object_path(&f.payload).unwrap().exists());
    let state=f.state();assert_eq!(state.environment_dependencies.as_ref().unwrap().environments.len(),1);assert_eq!(state.environment_dependencies.as_ref().unwrap().pins.len(),2);
    assert!(read(&a,a.root_data().clone()).is_ok());assert!(read(&b,b.root_data().clone()).is_ok());
    // Ordinary local retention has no permission to adopt the foreign graph.
    assert!(f.index.store.pin("ordinary-foreign",b.root_data()).is_err());assert!(f.index.store.unpin("dep-a").is_err());assert!(f.index.store.gc(Duration::ZERO).is_err());
}
#[test]
fn explicit_reattachment_needs_actual_original_parent_and_preserves_local_replica_obligation(){
    let f=fixture();let prep=prepared(&f);let env=boundary(&prep);let owner=publish(&f,env.clone(),"dep-reopen",b"source");let record=owner.record_data().clone();
    f.index.store.receive(&f.payload,&f.payload_bytes).unwrap();f.index.store.pin("ordinary-replica",&f.payload).unwrap();drop(owner);
    let reopened=LocalStore::open(f.index.store.root()).unwrap();let mut out=None;reopened.reattach_environment_dependent_retaining(&mut out,"dep-reopen".into(),env,Clock::new()).unwrap();
    let owner=out.as_ref().unwrap().reattached_owner().unwrap().clone();assert_eq!(owner.record_data(),&record);assert_eq!(read(&owner,f.payload.clone()).unwrap(),f.payload_bytes);
    let garbage=reopened.put_blob(b"unrelated garbage").unwrap();let mut gc=None;reopened.gc_environment_dependencies_retaining(&mut gc,vec![prep.acknowledgment().unwrap().retention().clone()],vec![owner],Duration::ZERO,Clock::new()).unwrap();
    assert!(gc.as_ref().unwrap().acknowledgment().unwrap().removed_objects>0);assert!(!reopened.object_path(&garbage).unwrap().exists());assert!(reopened.object_path(&f.payload).unwrap().exists());
}
#[test]
fn absent_dependent_or_parent_owner_causes_gc_zero_deletion(){
    let f=fixture();let prep=prepared(&f);let owner=publish(&f,boundary(&prep),"dep-missing",b"source");let garbage=f.index.store.put_blob(b"garbage must stay").unwrap();
    let mut old_gc=None;assert!(f.index.store.gc_backed_retaining(&mut old_gc,vec![prep.acknowledgment().unwrap().retention().clone()],Duration::ZERO,Clock::new()).is_err());
    assert_eq!(old_gc.as_ref().unwrap().partial_report_data().unwrap().removed_objects,0);drop(old_gc);
    let mut absent=None;assert!(f.index.store.gc_environment_dependencies_retaining(&mut absent,vec![],vec![owner],Duration::ZERO,Clock::new()).is_err());
    assert_eq!(absent.as_ref().unwrap().partial_report_data().unwrap().removed_objects,0);assert!(f.index.store.object_path(&garbage).unwrap().exists());
}
#[test]
fn changed_boundary_parent_record_cycle_and_depth_refuse_before_gc_delete(){
    for kind in 0..4{let f=fixture();let prep=prepared(&f);let owner=publish(&f,boundary(&prep),"dep-mutated",b"source");let garbage=f.index.store.put_blob(b"keep").unwrap();
        rewrite_state(&f,|state|{let deps=state.environment_dependencies.as_mut().unwrap();match kind{
            0=>deps.pins.get_mut("dep-mutated").unwrap().depth=3,
            1=>deps.environments.values_mut().next().unwrap().boundary=f.spec.base_runtime.clone(),
            2=>deps.environments.values_mut().next().unwrap().parent_record_sha256="0".repeat(64),
            _=>deps.environments.values_mut().next().unwrap().parent_pin="dep-mutated".into()}});
        let mut gc=None;assert!(f.index.store.gc_environment_dependencies_retaining(&mut gc,vec![prep.acknowledgment().unwrap().retention().clone()],vec![owner],Duration::ZERO,Clock::new()).is_err());
        assert!(f.index.store.object_path(&garbage).unwrap().exists());
    }
}
#[test]
fn real_after_preparation_mutation_is_not_certified_by_a_new_source_boundary(){
    let f=fixture();let prep=prepared(&f);let path=f.baked_root.join("cas/objects").join(&f.payload.digest[..2]).join(&f.payload.digest[2..]);
    fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'Q';f.payload_bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    let mut out=None;assert!(prep.derive_environment_subgraph_retaining(&mut out,Clock::new()).is_err());assert!(out.is_some());assert!(out.as_ref().unwrap().root_data().is_err());
}
#[test]
fn same_size_namespace_replacement_cannot_adopt_original_preparation_lineage(){
    let f=fixture();let prep=prepared(&f);let moved=f.temporary.path().join("original-index");fs::rename(&f.index.root,&moved).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&f.index.root).unwrap();let mut out=None;
    assert!(prep.derive_environment_subgraph_retaining(&mut out,Clock::new()).is_err());assert!(out.is_some());
}
struct CancelAfterPin{clock:Arc<Clock>,path:PathBuf,pin:String}
impl ImageImportFence for CancelAfterPin{fn remaining(&self)->Result<Duration>{
    if let Ok(bytes)=fs::read(&self.path){if let Ok(value)=serde_json::from_slice::<serde_json::Value>(&bytes){if !value["state"]["environment_dependencies"]["pins"][&self.pin].is_null(){self.clock.0.cancel();}}}
    self.clock.remaining()}}
#[test]
fn committed_dependent_record_is_not_promoted_after_late_ack(){
    let f=fixture();let prep=prepared(&f);let env=boundary(&prep);let stage=f.index.store.begin_staging(Duration::from_secs(120)).unwrap();let root=runnable(&f,env.root_data().unwrap(),b"source",&stage.id);let mut out=None;
    let fence=Arc::new(CancelAfterPin{clock:Clock::new(),path:f.index.store.root().join("retention.json"),pin:"late".into()});
    assert!(f.index.store.publish_environment_dependent_retaining(&mut out,stage.id,"late".into(),root,env,fence).is_err());
    assert!(out.as_ref().unwrap().acknowledgment().is_err());assert!(f.state().environment_dependencies.unwrap().pins.contains_key("late"));
}
#[test]
fn readonly_membership_does_not_convert_unknown_root_or_foreign_parent_into_inputs(){
    let f=fixture();let prep=prepared(&f);let env=boundary(&prep);let other=fixture();let mut publication=None;
    let stage=other.index.store.begin_staging(Duration::from_secs(120)).unwrap();let root=runnable(&other,env.root_data().unwrap(),b"foreign store",&stage.id);
    assert!(other.index.store.publish_environment_dependent_retaining(&mut publication,stage.id,"foreign".into(),root,env,Clock::new()).is_err());assert!(publication.as_ref().unwrap().acknowledgment().is_err());
    let owner=publish(&f,boundary(&prep),"known",b"source");let outside=f.index.store.put_blob(b"not retained here").unwrap();let mut failed=None;assert!(owner.read_retaining(&mut failed,outside,Clock::new()).is_err());assert!(failed.is_some());assert!(failed.as_ref().unwrap().verified_bytes().is_err());
    let inputs=RetainedRunnableInputs::Environment(owner);assert!(inputs.root_data().kind==ObjectKind::RunnableRoot);
}
