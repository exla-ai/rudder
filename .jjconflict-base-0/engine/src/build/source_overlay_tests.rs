//! Real CAS/SQLite source inheritance components, not project/Root/task grants.
//! No process, provider, compiler or model is launched by these components.
use super::*;
use crate::store::environment_dependencies::{RetainedSourceParent,SourceOverlayPublication,SourceOverlayPinOwner};
use crate::store::environment_dependencies::source::OverlayTestPoint;
use crate::content::{FileNode,FILE_LAYOUT};
struct SourceFixture{f:Fixture,prep:RetainedBackedPreparation,env:Arc<RetainedEnvironmentSubgraph>,base:Arc<EnvironmentDependentPinOwner>,
    parent:Arc<RetainedSourceParent>,inherited:ObjectRef,inherited_bytes:Vec<u8>,base_root:RunnableRoot}
fn source_fixture()->SourceFixture{
    let f=fixture();let prep=prepared(&f);let env=boundary(&prep);let clock=Clock::new();let store=&f.index.store;
    let stage=store.begin_staging(clock.remaining().unwrap()).unwrap();
    let template=runnable(&f,env.root_data().unwrap(),b"template",&stage.id);
    let base_root=crate::snapshot::decode_runnable(&store.read_object(&template).unwrap()).unwrap();
    let inherited_bytes=vec![b'I';4*1024*1024];let inherited=store.put_blob(&inherited_bytes).unwrap();let tail=store.put_blob(b"base-tail").unwrap();
    let root=child_root(&f,&base_root,&stage.id,&inherited,&tail);
    let mut publication=None;store.publish_environment_dependent_retaining(&mut publication,stage.id,"source-base".into(),root,env.clone(),clock).unwrap();
    let base=publication.as_ref().unwrap().acknowledgment().unwrap().clone();
    let base_root=crate::snapshot::decode_runnable(&store.read_object(base.root_data()).unwrap()).unwrap();
    let clock=Clock::new();let mut parent=None;base.derive_source_parent_retaining(&mut parent,clock.clone()).unwrap();clock.0.cancel();
    SourceFixture{f,prep,env,base,parent:parent.unwrap(),inherited,inherited_bytes,base_root}
}
fn child_root(f:&Fixture,base:&RunnableRoot,stage:&str,inherited:&ObjectRef,tail:&ObjectRef)->ObjectRef{
    let store=&f.index.store;let file=store.put_metadata(ObjectKind::File,&FileNode{format_version:1,layout:FILE_LAYOUT.into(),level:0,
        logical_size:inherited.size_bytes+tail.size_bytes,children:vec![inherited.clone(),tail.clone()]}).unwrap();
    let directory=PagedDirectory::new(&LocalDirectoryStore::new(store,stage)).build(0o755,vec![
        DirectoryEntry::new(b"real-file",EntryNode::File{object:file,logical_size:inherited.size_bytes+tail.size_bytes,mode:0o644}).unwrap()]).unwrap();
    let old=crate::snapshot::decode_source(&store.read_object(&base.source).unwrap()).unwrap();
    let source=store.put_metadata(ObjectKind::SourceRoot,&SourceV2{format_version:2,consistency:CheckpointConsistency::Quiesced,directory:directory.clone(),projection_policy:old.projection_policy}).unwrap();
    store.put_metadata(ObjectKind::RunnableRoot,&RunnableRoot{format_version:1,consistency:CheckpointConsistency::Quiesced,source,environment:base.environment.clone(),
        view_root:directory,profile:base.profile.clone(),hardlinks:base.hardlinks.clone(),knowledge_cursor:base.knowledge_cursor}).unwrap()
}
fn staged_child(frame:&SourceFixture,text:&[u8])->(Arc<Clock>,crate::store::StagingLease,ObjectRef,ObjectRef){
    let clock=Clock::new();let stage=frame.f.index.store.begin_staging(clock.remaining().unwrap()).unwrap();let tail=frame.f.index.store.put_blob(text).unwrap();
    let root=child_root(&frame.f,&frame.base_root,&stage.id,&frame.inherited,&tail);(clock,stage,root,tail)
}
fn publish_child(frame:&SourceFixture,name:&str,text:&[u8])->SourceOverlayPublication{
    let(clock,stage,root,_)=staged_child(frame,text);let mut out=None;frame.f.index.store.publish_source_overlay_retaining(&mut out,stage.id,name.into(),root,frame.parent.clone(),clock).unwrap();out.unwrap()
}
fn read_child(owner:&Arc<SourceOverlayPinOwner>,r:ObjectRef)->Result<Vec<u8>>{let mut out=None;owner.read_retaining(&mut out,r,Clock::new())?;Ok(out.unwrap().verified_bytes()?.to_vec())}

#[test]
fn genuine_source_parent_outlives_clock_and_changed_chunks_only_are_read_verified_and_synced(){
    let frame=source_fixture();let(clock,stage,root,tail)=staged_child(&frame,b"new-private-tail");let mut out=None;
    frame.f.index.store.publish_source_overlay_retaining(&mut out,stage.id,"child".into(),root.clone(),frame.parent.clone(),clock).unwrap();let held=out.as_ref().unwrap();let owner=held.acknowledgment().unwrap();
    // These counters record actual completed object reads/actual successful
    // sync_file calls in StoreIo, not inferred membership or a saved receipt.
    assert!(!held.io_reads_data().contains_key(&frame.inherited));assert!(!held.io_syncs_data().contains_key(&frame.inherited));
    assert_eq!(held.io_reads_data().get(&tail),Some(&tail.size_bytes));assert_eq!(held.io_syncs_data().get(&tail),Some(&1));
    for(r,n)in held.io_reads_data(){assert!(*n>0);assert_eq!(held.io_syncs_data().get(r),Some(&1));}
    let work=held.work_data();assert!(work.actual_io_complete);assert_eq!(work.actual_local_hash_reads,held.io_reads_data().len()as u64);assert_eq!(work.actual_local_hash_bytes,held.io_reads_data().values().sum::<u64>());
    assert_eq!(work.actual_local_object_syncs,held.io_syncs_data().values().sum::<u64>());assert_eq!(work.actual_local_object_sync_bytes,held.io_syncs_data().iter().map(|(r,n)|r.size_bytes*n).sum::<u64>());
    assert_eq!(work.actual_local_hash_reads,work.verified_delta_objects);assert_eq!(work.actual_local_hash_bytes,work.verified_delta_bytes);assert_eq!(work.actual_local_object_syncs,work.actual_local_hash_reads);assert_eq!(work.actual_local_object_sync_bytes,work.actual_local_hash_bytes);
    assert!(held.work_data().verified_delta_bytes<128*1024);assert!(held.work_data().reused_parent_edges>0);
    assert_eq!(owner.root_data(),&root);assert!(Arc::ptr_eq(owner.source_parent(),&frame.parent));assert_eq!(owner.record_data().depth,3);
    assert_eq!(owner.pin_name_data(),"child");assert_eq!(owner.store_root_data(),frame.f.index.store.root());
    assert!(owner.contains_data(&tail,&*Clock::new()).unwrap());assert!(owner.contains_data(&frame.inherited,&*Clock::new()).unwrap());assert!(owner.contains_data(&frame.f.payload,&*Clock::new()).unwrap());
    assert!(owner.matches_record_data(&frame.f.state(),"child"));assert!(!owner.matches_record_data(&frame.f.state(),"source-base"));
    let outside=frame.f.index.store.put_blob(b"outside owner membership").unwrap();assert!(!owner.contains_data(&outside,&*Clock::new()).unwrap());
    assert_eq!(read_child(owner,frame.inherited.clone()).unwrap(),frame.inherited_bytes);
    let actual=crate::snapshot::decode_runnable(&read_child(owner,root).unwrap()).unwrap();assert_eq!(actual.environment,frame.base_root.environment);
    assert_eq!(frame.f.state().environment_dependencies.as_ref().unwrap().pins.get("source-base"),Some(frame.base.record_data()));
    assert!(!frame.f.index.store.object_path(&frame.f.payload).unwrap().exists());
}

#[test]
fn fifty_siblings_share_one_source_parent_without_an_ancestry_chain(){
    let frame=source_fixture();let mut held=Vec::new();let mut roots=BTreeSet::new();
    for n in 0..50{let original=publish_child(&frame,&format!("child:{n}"),format!("actual private tail {n}").as_bytes());let owner=original.acknowledgment().unwrap();
        assert!(Arc::ptr_eq(owner.source_parent(),&frame.parent));assert!(roots.insert(owner.root_data().clone()));assert_eq!(owner.record_data().depth,3);
        assert!(!original.io_reads_data().contains_key(&frame.inherited));assert!(!original.io_syncs_data().contains_key(&frame.inherited));held.push(original);}
    let state=frame.f.state();let source=state.source_dependencies.as_ref().unwrap();assert_eq!(source.parents.len(),1);assert_eq!(source.pins.len(),50);assert!(source.holds.is_empty());
    assert_eq!(state.environment_dependencies.as_ref().unwrap().pins.len(),1);assert!(state.pins.values().all(|r|!roots.contains(r)));
    assert!(frame.f.index.store.unpin("source-base").is_err());assert!(frame.f.index.store.unpin("child:0").is_err());
    let garbage=frame.f.index.store.put_blob(b"must survive missing owner GC").unwrap();let mut old=None;
    assert!(frame.f.index.store.gc_environment_dependencies_retaining(&mut old,vec![frame.prep.acknowledgment().unwrap().retention().clone()],vec![frame.base.clone()],Duration::ZERO,Clock::new()).is_err());
    assert_eq!(old.as_ref().unwrap().partial_report_data().unwrap().removed_objects,0);assert!(frame.f.index.store.object_path(&garbage).unwrap().exists());
}

#[test]
fn changed_chunk_corruption_is_not_reused_and_does_not_fence_the_shared_parent(){
    let frame=source_fixture();let(clock,stage,root,tail)=staged_child(&frame,b"bad new private bytes");let path=frame.f.index.store.object_path(&tail).unwrap();
    fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'!';tail.size_bytes as usize]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    let mut out=None;assert!(frame.f.index.store.publish_source_overlay_retaining(&mut out,stage.id,"bad-private".into(),root,frame.parent.clone(),clock).is_err());
    assert!(out.as_ref().unwrap().acknowledgment().is_err());assert_eq!(out.as_ref().unwrap().io_reads_data().get(&tail),Some(&tail.size_bytes));assert!(!out.as_ref().unwrap().io_syncs_data().contains_key(&tail));
    assert!(!frame.f.state().source_dependencies.unwrap().holds.is_empty());
    // Simulated failed-process FD loss, NOT a normal retirement receipt. The
    // persistent unknown hold remains, and no original ACK is reconstructed.
    drop(out);let mut read=None;frame.base.read_retaining(&mut read,frame.base.root_data().clone(),Clock::new()).unwrap();read.as_ref().unwrap().verified_bytes().unwrap();
}

#[test]
fn inherited_corruption_is_detected_on_served_read_and_observed_failure_stays_absorbing(){
    let frame=source_fixture();let publication=publish_child(&frame,"child",b"tail");let owner=publication.acknowledgment().unwrap();let path=frame.f.index.store.object_path(&frame.inherited).unwrap();
    fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'?';frame.inherited_bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    let mut failed=None;assert!(owner.read_retaining(&mut failed,frame.inherited.clone(),Clock::new()).is_err());assert!(failed.as_ref().unwrap().verified_bytes().is_err());drop(failed);
    fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,&frame.inherited_bytes).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    let mut again=None;assert!(owner.read_retaining(&mut again,owner.root_data().clone(),Clock::new()).is_err());assert!(again.as_ref().unwrap().verified_bytes().is_err());
}

#[test]
fn unknown_hold_sync_and_late_ack_cannot_be_replayed_or_readback_promoted(){
    for point in [OverlayTestPoint::AfterHold,OverlayTestPoint::FinalRename,OverlayTestPoint::AfterCommit]{
        let frame=source_fixture();let(clock,stage,root,_)=staged_child(&frame,b"unknown");let mut out=None;
        assert!(frame.f.index.store.publish_source_overlay_fault_retaining(&mut out,stage.id,"unknown".into(),root,frame.parent.clone(),clock,point).is_err());
        assert!(out.as_ref().unwrap().acknowledgment().is_err());assert!(out.as_ref().unwrap().failure().is_some());
        let work=out.as_ref().unwrap().work_data();assert!(!work.actual_io_complete);assert_eq!((work.actual_local_hash_reads,work.actual_local_hash_bytes,work.actual_local_object_syncs,work.actual_local_object_sync_bytes),(0,0,0,0));
        let state_before=frame.f.state();assert!(frame.f.index.store.publish_source_overlay_retaining(&mut out,"never-replay".into(),"unknown".into(),frame.base.root_data().clone(),frame.parent.clone(),Clock::new()).is_err());assert_eq!(frame.f.state(),state_before);
        let mut no_replay=None;let cancelled=Clock::new();cancelled.0.cancel();assert!(frame.f.index.store.reattach_source_overlay_retaining(&mut no_replay,"unknown".into(),frame.parent.clone(),cancelled).is_err());
        assert!(no_replay.as_ref().unwrap().reattached_owner().is_err());
    }
}

#[test]
fn reopen_needs_genuine_fully_revalidated_parent_and_every_actual_owner_for_gc(){
    let frame=source_fixture();let publication=publish_child(&frame,"child",b"persisted");let record=publication.acknowledgment().unwrap().record_data().clone();drop(publication);
    let reopened=crate::store::LocalStore::open(frame.f.index.store.root()).unwrap();let garbage=reopened.put_blob(b"real unreachable payload").unwrap();let mut absent=None;
    assert!(reopened.gc_source_overlays_retaining(&mut absent,vec![frame.prep.acknowledgment().unwrap().retention().clone()],vec![frame.base.clone()],vec![],Duration::ZERO,Clock::new()).is_err());
    assert_eq!(absent.as_ref().unwrap().partial_report_data().unwrap().removed_objects,0);assert!(reopened.object_path(&garbage).unwrap().exists());drop(absent);
    let mut flat=None;reopened.reattach_environment_dependent_retaining(&mut flat,"source-base".into(),frame.env.clone(),Clock::new()).unwrap();let base=flat.as_ref().unwrap().reattached_owner().unwrap().clone();
    let mut parent=None;base.derive_source_parent_retaining(&mut parent,Clock::new()).unwrap();let mut child=None;
    reopened.reattach_source_overlay_retaining(&mut child,"child".into(),parent.unwrap(),Clock::new()).unwrap();let owner=child.as_ref().unwrap().reattached_owner().unwrap().clone();assert_eq!(owner.record_data(),&record);
    assert_eq!(read_child(&owner,frame.inherited.clone()).unwrap(),frame.inherited_bytes);let mut gc=None;
    reopened.gc_source_overlays_retaining(&mut gc,vec![frame.prep.acknowledgment().unwrap().retention().clone()],vec![base],vec![owner],Duration::ZERO,Clock::new()).unwrap();
    assert!(gc.as_ref().unwrap().acknowledgment().unwrap().removed_objects>0);assert!(!reopened.object_path(&garbage).unwrap().exists());assert!(reopened.object_path(&frame.inherited).unwrap().exists());
}

#[test]
fn source_overlay_parent_record_mutation_or_ancestry_cycle_refuses_before_gc_deletion(){
    for kind in 0..3{let frame=source_fixture();let publication=publish_child(&frame,"child",b"mutation");let child=publication.acknowledgment().unwrap().clone();let garbage=frame.f.index.store.put_blob(b"keep on corruption").unwrap();
        rewrite_state(&frame.f,|state|{let source=state.source_dependencies.as_mut().unwrap();match kind{
            0=>source.pins.get_mut("child").unwrap().depth=4,
            1=>source.parents.values_mut().next().unwrap().parent_pin="child".into(),
            _=>state.environment_dependencies.as_mut().unwrap().pins.get_mut("source-base").unwrap().local_graph_sha256="0".repeat(64)}});
        let mut gc=None;assert!(frame.f.index.store.gc_source_overlays_retaining(&mut gc,vec![frame.prep.acknowledgment().unwrap().retention().clone()],vec![frame.base.clone()],vec![child],Duration::ZERO,Clock::new()).is_err());
        assert!(frame.f.index.store.object_path(&garbage).unwrap().exists());
    }
}

#[test]
fn another_genuine_parent_or_another_store_cannot_reattach_an_overlay(){
    let frame=source_fixture();let publication=publish_child(&frame,"child",b"specific parent");
    let second=publish(&frame.f,frame.env.clone(),"other-flat-source",b"other genuinely verified parent");
    let mut second_full=None;frame.f.index.store.reattach_environment_dependent_retaining(&mut second_full,"other-flat-source".into(),frame.env.clone(),Clock::new()).unwrap();
    let mut second_parent=None;second_full.as_ref().unwrap().reattached_owner().unwrap().derive_source_parent_retaining(&mut second_parent,Clock::new()).unwrap();
    assert_eq!(second.root_data(),second_parent.as_ref().unwrap().original_owner().root_data());
    let mut wrong=None;assert!(frame.f.index.store.reattach_source_overlay_retaining(&mut wrong,"child".into(),second_parent.unwrap(),Clock::new()).is_err());
    assert!(wrong.as_ref().unwrap().reattached_owner().is_err());drop(wrong);
    let elsewhere=fixture();let clock=Clock::new();let stage=elsewhere.index.store.begin_staging(clock.remaining().unwrap()).unwrap();let mut wrong_store=None;
    assert!(elsewhere.index.store.publish_source_overlay_retaining(&mut wrong_store,stage.id,"foreign".into(),publication.acknowledgment().unwrap().root_data().clone(),frame.parent.clone(),clock).is_err());
    assert!(wrong_store.as_ref().unwrap().acknowledgment().is_err());assert!(elsewhere.state().source_dependencies.is_none());
    assert_eq!(read_child(publication.acknowledgment().unwrap(),frame.inherited.clone()).unwrap(),frame.inherited_bytes);
}

#[test]
fn inherited_immutable_fact_is_explicitly_not_a_fresh_cold_leaf_scrub(){
    let frame=source_fixture();let path=frame.f.index.store.object_path(&frame.inherited).unwrap();
    fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'!';frame.inherited_bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    // External mutation violates immutable CAS. A sibling ACK inherits the
    // already completed parent fact and never claims to rehash this cold leaf.
    let publication=publish_child(&frame,"inherits-completed-fact",b"changed bytes");
    assert!(!publication.io_reads_data().contains_key(&frame.inherited));assert!(!publication.io_syncs_data().contains_key(&frame.inherited));
    let mut actual_read=None;assert!(publication.acknowledgment().unwrap().read_retaining(&mut actual_read,frame.inherited.clone(),Clock::new()).is_err());
    assert!(actual_read.as_ref().unwrap().verified_bytes().is_err());
}

#[test]
fn missing_or_corrupt_cold_parent_blocks_full_reattachment_and_gc_before_delete(){
    for missing in [false,true]{
        let frame=source_fixture();let publication=publish_child(&frame,"child",b"cold");let owner=publication.acknowledgment().unwrap().clone();
        let garbage=frame.f.index.store.put_blob(b"retain when cold parent is damaged").unwrap();let path=frame.f.index.store.object_path(&frame.inherited).unwrap();
        if missing{fs::remove_file(&path).unwrap();}else{fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'!';frame.inherited_bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();}
        let reopened=LocalStore::open(frame.f.index.store.root()).unwrap();let mut flat=None;
        assert!(reopened.reattach_environment_dependent_retaining(&mut flat,"source-base".into(),frame.env.clone(),Clock::new()).is_err());assert!(flat.as_ref().unwrap().reattached_owner().is_err());drop(flat);
        let mut gc=None;assert!(reopened.gc_source_overlays_retaining(&mut gc,vec![frame.prep.acknowledgment().unwrap().retention().clone()],vec![frame.base.clone()],vec![owner.clone()],Duration::ZERO,Clock::new()).is_err());
        assert_eq!(gc.as_ref().unwrap().partial_report_data().unwrap().removed_objects,0);assert!(reopened.object_path(&garbage).unwrap().exists());drop(gc);
        // A physical failure observed by GC is absorbing for the original
        // source owner. Restoring bytes cannot mint another sibling ACK.
        if !missing{fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();}
        fs::write(&path,&frame.inherited_bytes).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
        let mut read=None;assert!(owner.read_retaining(&mut read,owner.root_data().clone(),Clock::new()).is_err());assert!(read.as_ref().unwrap().verified_bytes().is_err());
    }
}

#[test]
fn a_warm_original_parent_cannot_skip_explicit_cold_parent_reattachment(){
    let frame=source_fixture();let publication=publish_child(&frame,"child",b"known");let record=publication.acknowledgment().unwrap().record_data().clone();
    let mut warm=None;assert!(frame.f.index.store.reattach_source_overlay_retaining(&mut warm,"child".into(),frame.parent.clone(),Clock::new()).is_err());
    assert!(warm.as_ref().unwrap().reattached_owner().is_err());drop(warm);
    assert_eq!(publication.acknowledgment().unwrap().record_data(),&record);
    assert_eq!(read_child(publication.acknowledgment().unwrap(),frame.inherited.clone()).unwrap(),frame.inherited_bytes);
}

#[test]
fn existing_and_overlay_pins_share_the_original_1024_pin_capacity(){
    let frame=source_fixture();let first=publish_child(&frame,"child",b"one real overlay");
    // Retention DATA aliases a real fully verified CAS root to exercise record
    // capacity. These aliases are never represented as actual owner receipts.
    rewrite_state(&frame.f,|state|{let deps=state.environment_dependencies.as_mut().unwrap();let record=deps.pins["source-base"].clone();for n in 0..1021{deps.pins.insert(format!("capacity:{n}"),record.clone());}});
    let clock=Clock::new();let stage=frame.f.index.store.begin_staging(clock.remaining().unwrap()).unwrap();let mut at_limit=None;
    frame.f.index.store.publish_environment_dependent_retaining(&mut at_limit,stage.id,"last-flat".into(),frame.base.root_data().clone(),frame.env.clone(),clock).unwrap();
    let state=frame.f.state();assert_eq!(state.environment_dependencies.as_ref().unwrap().pins.len()+state.source_dependencies.as_ref().unwrap().pins.len(),1024);
    let(clock,stage,root,_)=staged_child(&frame,b"too many source pins");let mut overflow=None;
    assert!(frame.f.index.store.publish_source_overlay_retaining(&mut overflow,stage.id,"overflow-overlay".into(),root,frame.parent.clone(),clock).is_err());assert!(overflow.as_ref().unwrap().acknowledgment().is_err());
    assert!(frame.f.state().source_dependencies.as_ref().unwrap().holds.is_empty());drop(overflow);
    let clock=Clock::new();let stage=frame.f.index.store.begin_staging(clock.remaining().unwrap()).unwrap();let mut ordinary_overflow=None;
    assert!(frame.f.index.store.publish_environment_dependent_retaining(&mut ordinary_overflow,stage.id,"overflow-flat".into(),frame.base.root_data().clone(),frame.env.clone(),clock).is_err());
    assert!(frame.f.state().environment_dependencies.as_ref().unwrap().holds.is_empty());drop(ordinary_overflow);
    assert_eq!(read_child(first.acknowledgment().unwrap(),frame.inherited.clone()).unwrap(),frame.inherited_bytes);
}

struct CancelAfterDependencyHold{clock:Arc<Clock>,path:PathBuf,pin:String}
impl ImageImportFence for CancelAfterDependencyHold{fn remaining(&self)->Result<Duration>{
    if let Ok(raw)=fs::read(&self.path){if let Ok(value)=serde_json::from_slice::<serde_json::Value>(&raw){
        if value["state"]["environment_dependencies"]["holds"].as_object().is_some_and(|holds|holds.values().any(|h|h["pin"].as_str()==Some(self.pin.as_str()))){self.clock.0.cancel();}
    }}self.clock.remaining()
}}
#[test]
fn existing_and_overlay_unknown_holds_share_the_original_16_hold_capacity(){
    let frame=source_fixture();
    for n in 0..8{let clock=Clock::new();let stage=frame.f.index.store.begin_staging(clock.remaining().unwrap()).unwrap();let pin=format!("flat-unknown:{n}");
        let fence=Arc::new(CancelAfterDependencyHold{clock,path:frame.f.index.store.root().join("retention.json"),pin:pin.clone()});let mut held=None;
        assert!(frame.f.index.store.publish_environment_dependent_retaining(&mut held,stage.id,pin,frame.base.root_data().clone(),frame.env.clone(),fence).is_err());assert!(held.as_ref().unwrap().acknowledgment().is_err());
        assert_eq!(frame.f.state().environment_dependencies.as_ref().unwrap().holds.len(),n+1);drop(held);
    }
    for n in 0..8{let clock=Clock::new();let stage=frame.f.index.store.begin_staging(clock.remaining().unwrap()).unwrap();let mut held=None;
        assert!(frame.f.index.store.publish_source_overlay_fault_retaining(&mut held,stage.id,format!("overlay-unknown:{n}"),frame.base.root_data().clone(),frame.parent.clone(),clock,OverlayTestPoint::AfterHold).is_err());
        assert!(held.as_ref().unwrap().acknowledgment().is_err());assert_eq!(frame.f.state().source_dependencies.as_ref().unwrap().holds.len(),n+1);drop(held);
    }
    let state=frame.f.state();assert_eq!(state.environment_dependencies.as_ref().unwrap().holds.len()+state.source_dependencies.as_ref().unwrap().holds.len(),16);
    for overlay in [false,true]{let clock=Clock::new();let stage=frame.f.index.store.begin_staging(clock.remaining().unwrap()).unwrap();
        if overlay{let mut held=None;assert!(frame.f.index.store.publish_source_overlay_retaining(&mut held,stage.id,"one-hold-too-many".into(),frame.base.root_data().clone(),frame.parent.clone(),clock).is_err());drop(held);}
        else{let mut held=None;assert!(frame.f.index.store.publish_environment_dependent_retaining(&mut held,stage.id,"one-hold-too-many".into(),frame.base.root_data().clone(),frame.env.clone(),clock).is_err());drop(held);}
        let after=frame.f.state();assert_eq!(after.environment_dependencies.as_ref().unwrap().holds.len()+after.source_dependencies.as_ref().unwrap().holds.len(),16);
    }
    // Failed-process descriptor loss in this fixture does not remove any of
    // the persistent unknown holds or reconstitute the original publications.
}

#[derive(serde::Deserialize)]#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PriorRetentionData{format_version:u32,generation:u64,pins:BTreeMap<String,ObjectRef>,staging:BTreeMap<String,crate::store::StagingLease>,
    #[serde(default)]image_import_holds:BTreeMap<String,crate::store::ImageImportHold>,
    #[serde(default)]backed_retention:Option<crate::store::backed_data::BackedRetentionData>,
    #[serde(default)]environment_dependencies:Option<crate::store::environment_dependency_data::EnvironmentDependenciesData>}
#[test]
fn old_strict_reader_refuses_and_current_writers_preserve_source_dependencies(){
    let frame=source_fixture();let before=serde_json::to_value(frame.f.state()).unwrap();assert!(serde_json::from_value::<PriorRetentionData>(before).is_ok());
    let publication=publish_child(&frame,"child",b"keep extension");let source=frame.f.state().source_dependencies.unwrap();
    assert!(serde_json::from_value::<PriorRetentionData>(serde_json::to_value(frame.f.state()).unwrap()).is_err());
    let stage=frame.f.index.store.begin_staging(Duration::from_secs(30)).unwrap();frame.f.index.store.renew_staging(&stage.id,Duration::from_secs(30)).unwrap();frame.f.index.store.abandon_staging(&stage.id).unwrap();
    let blob=frame.f.index.store.put_blob(b"ordinary root").unwrap();frame.f.index.store.pin("ordinary",&blob).unwrap();frame.f.index.store.pin_existing("ordinary-fork",&blob).unwrap();frame.f.index.store.unpin("ordinary").unwrap();
    let flat=publish(&frame.f,frame.env.clone(),"another-flat",b"ordinary current writer");assert_eq!(frame.f.state().source_dependencies.as_ref(),Some(&source));
    assert!(frame.f.index.store.pin("child",&blob).is_err());assert!(frame.f.index.store.unpin("source-base").is_err());assert_eq!(frame.f.state().source_dependencies.as_ref(),Some(&source));
    let garbage=frame.f.index.store.put_blob(b"ordinary GC must refuse").unwrap();assert!(frame.f.index.store.gc(Duration::ZERO).is_err());assert!(frame.f.index.store.object_path(&garbage).unwrap().exists());
    assert_eq!(read_child(publication.acknowledgment().unwrap(),frame.inherited.clone()).unwrap(),frame.inherited_bytes);assert!(flat.root_data()!=frame.base.root_data());
}

#[test]
fn clock_cancellation_is_unknown_but_a_prior_observed_physical_failure_is_absorbing(){
    for missing in [false,true]{let frame=source_fixture();let publication=publish_child(&frame,"child",b"cancel");let owner=publication.acknowledgment().unwrap();
        // Cancellation before any actual read cannot condemn immutable data.
        let cancelled=Clock::new();cancelled.0.cancel();let mut no_read=None;assert!(owner.read_retaining(&mut no_read,frame.inherited.clone(),cancelled).is_err());assert!(no_read.as_ref().unwrap().verified_bytes().is_err());drop(no_read);
        assert_eq!(read_child(owner,frame.inherited.clone()).unwrap(),frame.inherited_bytes);
        let path=frame.f.index.store.object_path(&frame.inherited).unwrap();if missing{fs::remove_file(&path).unwrap();}else{fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'?';frame.inherited_bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();}
        let clock=Clock::new();let mut physical=None;assert!(owner.read_retaining(&mut physical,frame.inherited.clone(),clock.clone()).is_err());clock.0.cancel();assert!(physical.as_ref().unwrap().verified_bytes().is_err());drop(physical);
        if !missing{fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();}fs::write(&path,&frame.inherited_bytes).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
        let mut after=None;assert!(owner.read_retaining(&mut after,owner.root_data().clone(),Clock::new()).is_err());assert!(after.as_ref().unwrap().verified_bytes().is_err());
    }
}

fn root_at_directory(frame:&SourceFixture,directory:ObjectRef)->ObjectRef{
    let store=&frame.f.index.store;let old=crate::snapshot::decode_source(&store.read_object(&frame.base_root.source).unwrap()).unwrap();
    let source=store.put_metadata(ObjectKind::SourceRoot,&SourceV2{format_version:2,consistency:CheckpointConsistency::Quiesced,directory:directory.clone(),projection_policy:old.projection_policy}).unwrap();
    store.put_metadata(ObjectKind::RunnableRoot,&RunnableRoot{format_version:1,consistency:CheckpointConsistency::Quiesced,source,environment:frame.base_root.environment.clone(),
        view_root:directory,profile:frame.base_root.profile.clone(),hardlinks:frame.base_root.hardlinks.clone(),knowledge_cursor:0}).unwrap()
}
#[test]
fn reused_directory_aliases_charge_every_logical_path_at_the_384_mib_boundary(){
    let frame=source_fixture();
    for count in [95,96]{let clock=Clock::new();let stage=frame.f.index.store.begin_staging(clock.remaining().unwrap()).unwrap();
        let entries: Vec<_>=(0..count).map(|n|DirectoryEntry::new(format!("alias-{n:03}").as_bytes(),EntryNode::Directory{object:frame.parent.binding_data().unwrap().directory.clone()}).unwrap()).collect();
        let directory=PagedDirectory::new(&LocalDirectoryStore::new(&frame.f.index.store,&stage.id)).build(0o755,entries).unwrap();let root=root_at_directory(&frame,directory);let mut out=None;
        let result=frame.f.index.store.publish_source_overlay_retaining(&mut out,stage.id,format!("aliases:{count}"),root,frame.parent.clone(),clock);
        assert!(!out.as_ref().unwrap().io_reads_data().contains_key(&frame.inherited));
        if count==95{result.unwrap();assert_eq!(out.as_ref().unwrap().work_data().source_entries,190);assert_eq!(out.as_ref().unwrap().work_data().source_logical_bytes,95*(frame.inherited.size_bytes+9));}
        else{assert!(result.is_err());assert!(out.as_ref().unwrap().acknowledgment().is_err());}
    }
}
#[test]
fn changed_metadata_cannot_exceed_256_directory_levels_or_lie_about_reused_children(){
    for scenario in 0..4{let frame=source_fixture();let clock=Clock::new();let stage=frame.f.index.store.begin_staging(clock.remaining().unwrap()).unwrap();
        let store=&frame.f.index.store;let writer=LocalDirectoryStore::new(store,&stage.id);
        let directory=if scenario<=1{let mut directory=frame.parent.binding_data().unwrap().directory.clone();for _ in 0..(256+scenario){directory=PagedDirectory::new(&writer).build(0o755,vec![DirectoryEntry::new(b"nested",EntryNode::Directory{object:directory}).unwrap()]).unwrap();}directory}
            else if scenario==2{let mut original=crate::directory::decode_directory_root(&store.read_object(&frame.parent.binding_data().unwrap().directory).unwrap()).unwrap();original.entry_count+=1;store.put_metadata(ObjectKind::Directory,&original).unwrap()}
            else{let original=crate::directory::decode_directory_root(&store.read_object(&frame.parent.binding_data().unwrap().directory).unwrap()).unwrap();let shard=crate::directory::decode_shard(&store.read_object(&original.directory_shard).unwrap()).unwrap();
                let crate::directory::ShardNode::Leaf{entries}=shard.node else{panic!("fixture must be one leaf")};let EntryNode::File{object,..}=&entries[0].node else{panic!("fixture must contain file descriptor")};
                PagedDirectory::new(&writer).build(0o755,vec![DirectoryEntry::new(b"wrong-length",EntryNode::File{object:object.clone(),logical_size:1,mode:0o644}).unwrap()]).unwrap()};
        let root=root_at_directory(&frame,directory);let mut out=None;let result=store.publish_source_overlay_retaining(&mut out,stage.id,"bounded-child".into(),root,frame.parent.clone(),clock);
        if scenario==0{result.unwrap();assert_eq!(out.as_ref().unwrap().acknowledgment().unwrap().record_data().source_height,256);}
        else{assert!(result.is_err());assert!(out.as_ref().unwrap().acknowledgment().is_err());}
        assert!(!out.as_ref().unwrap().io_reads_data().contains_key(&frame.inherited));
    }
}

fn replace_fixture_parent(frame:&mut SourceFixture,directory:ObjectRef,stage:crate::store::StagingLease,clock:Arc<Clock>){
    let root=root_at_directory(frame,directory);let mut out=None;
    frame.f.index.store.publish_environment_dependent_retaining(&mut out,stage.id,"wide-source-base".into(),root,frame.env.clone(),clock).unwrap();
    frame.base=out.as_ref().unwrap().acknowledgment().unwrap().clone();
    frame.base_root=crate::snapshot::decode_runnable(&frame.f.index.store.read_object(frame.base.root_data()).unwrap()).unwrap();
    let mut parent=None;frame.base.derive_source_parent_retaining(&mut parent,Clock::new()).unwrap();frame.parent=parent.unwrap();
}
fn wide_parent(frame:&mut SourceFixture,count:usize)->ObjectRef{
    let clock=Clock::new();let store=&frame.f.index.store;let stage=store.begin_staging(clock.remaining().unwrap()).unwrap();let tiny=store.put_blob(b"tiny").unwrap();
    let entries: Vec<_>=(0..count).map(|n|DirectoryEntry::new(format!("name-{n:05}").as_bytes(),EntryNode::File{object:tiny.clone(),logical_size:tiny.size_bytes,mode:0o644}).unwrap()).collect();
    let directory=PagedDirectory::new(&LocalDirectoryStore::new(store,&stage.id)).build(0o755,entries).unwrap();replace_fixture_parent(frame,directory,stage,clock);tiny
}
fn actual_shards(store:&LocalStore,directory:&ObjectRef)->BTreeSet<ObjectRef>{
    let root=crate::directory::decode_directory_root(&store.read_object(directory).unwrap()).unwrap();let mut pending=vec![root.directory_shard];let mut shards=BTreeSet::new();
    while let Some(r)=pending.pop(){if !shards.insert(r.clone()){continue;}let shard=crate::directory::decode_shard(&store.read_object(&r).unwrap()).unwrap();
        if let crate::directory::ShardNode::Branch{children}=shard.node{pending.extend(children.into_iter().map(|c|c.shard.reference()));}}
    shards
}
#[test]
fn a_changed_radix_path_reuses_exact_parent_shards_without_reading_or_syncing_them(){
    let mut frame=source_fixture();let tiny=wide_parent(&mut frame,4096);let directory=frame.parent.binding_data().unwrap().directory.clone();let shards=actual_shards(&frame.f.index.store,&directory);assert!(shards.len()>4);
    let clock=Clock::new();let store=&frame.f.index.store;let stage=store.begin_staging(clock.remaining().unwrap()).unwrap();let changed=store.put_blob(b"one changed radix leaf").unwrap();
    let update=PagedDirectory::new(&LocalDirectoryStore::new(store,&stage.id)).update(&directory,b"name-02048",Some(EntryNode::File{object:changed.clone(),logical_size:changed.size_bytes,mode:0o644})).unwrap();
    let updated=actual_shards(store,&update.root);assert!(shards.intersection(&updated).count()>0);assert!(shards.difference(&updated).count()>0);
    let root=root_at_directory(&frame,update.root);let mut publication=None;store.publish_source_overlay_retaining(&mut publication,stage.id,"radix-child".into(),root,frame.parent.clone(),clock).unwrap();let held=publication.as_ref().unwrap();
    for shard in &shards{assert!(!held.io_reads_data().contains_key(shard));assert!(!held.io_syncs_data().contains_key(shard));}
    assert!(!held.io_reads_data().contains_key(&tiny));assert!(!held.io_syncs_data().contains_key(&tiny));assert_eq!(held.io_reads_data().get(&changed),Some(&changed.size_bytes));assert_eq!(held.io_syncs_data().get(&changed),Some(&1));
    assert_eq!(held.work_data().source_entries,4096);assert_eq!(held.work_data().source_logical_bytes,4095*tiny.size_bytes+changed.size_bytes);assert!(held.work_data().reused_parent_edges>0);
    assert_eq!(read_child(held.acknowledgment().unwrap(),tiny).unwrap(),b"tiny");
}
#[test]
fn repeated_small_directory_aliases_cannot_evade_the_100000_path_limit(){
    let mut frame=source_fixture();let tiny=wide_parent(&mut frame,1000);
    for count in [99u64,100]{let clock=Clock::new();let store=&frame.f.index.store;let stage=store.begin_staging(clock.remaining().unwrap()).unwrap();
        let entries: Vec<_>=(0..count).map(|n|DirectoryEntry::new(format!("alias-{n:03}").as_bytes(),EntryNode::Directory{object:frame.parent.binding_data().unwrap().directory.clone()}).unwrap()).collect();
        let directory=PagedDirectory::new(&LocalDirectoryStore::new(store,&stage.id)).build(0o755,entries).unwrap();let root=root_at_directory(&frame,directory);let mut out=None;
        let result=store.publish_source_overlay_retaining(&mut out,stage.id,format!("path-count:{count}"),root,frame.parent.clone(),clock);let held=out.as_ref().unwrap();assert!(!held.io_reads_data().contains_key(&tiny));
        if count==99{result.unwrap();assert_eq!(held.work_data().source_entries,99099);assert_eq!(held.work_data().source_logical_bytes,99000*tiny.size_bytes);}
        else{assert!(result.is_err());assert!(held.acknowledgment().is_err());}
    }
}
#[test]
fn a_missing_or_corrupt_private_delta_observed_by_gc_is_absorbing_only_for_that_child(){
    for missing in [false,true]{let frame=source_fixture();let(clock,stage,root,tail)=staged_child(&frame,b"private GC obligation");let mut publication=None;
        frame.f.index.store.publish_source_overlay_retaining(&mut publication,stage.id,"gc-child".into(),root,frame.parent.clone(),clock).unwrap();let owner=publication.as_ref().unwrap().acknowledgment().unwrap().clone();
        let bytes=frame.f.index.store.read_object(&tail).unwrap();let path=frame.f.index.store.object_path(&tail).unwrap();
        if missing{fs::remove_file(&path).unwrap();}else{fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'?';bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();}
        let garbage=frame.f.index.store.put_blob(b"no deletion before exact delta validation").unwrap();let mut gc=None;
        assert!(frame.f.index.store.gc_source_overlays_retaining(&mut gc,vec![frame.prep.acknowledgment().unwrap().retention().clone()],vec![frame.base.clone()],vec![owner.clone()],Duration::ZERO,Clock::new()).is_err());
        assert_eq!(gc.as_ref().unwrap().partial_report_data().unwrap().removed_objects,0);assert!(frame.f.index.store.object_path(&garbage).unwrap().exists());drop(gc);
        if !missing{fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();}fs::write(&path,&bytes).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
        let mut read=None;assert!(owner.read_retaining(&mut read,owner.root_data().clone(),Clock::new()).is_err());assert!(read.as_ref().unwrap().verified_bytes().is_err());drop(read);
        let sibling=publish_child(&frame,"healthy-sibling",b"independent private delta");assert_eq!(read_child(sibling.acknowledgment().unwrap(),frame.inherited.clone()).unwrap(),frame.inherited_bytes);
    }
}
#[test]
fn cancellation_after_a_tagged_physical_observation_cannot_erase_the_failure(){
    for inherited in [false,true]{let frame=source_fixture();let(clock,stage,root,tail)=staged_child(&frame,b"boundary delta");let mut publication=None;
        frame.f.index.store.publish_source_overlay_retaining(&mut publication,stage.id,"boundary-child".into(),root,frame.parent.clone(),clock).unwrap();let owner=publication.as_ref().unwrap().acknowledgment().unwrap();
        let reference=if inherited{frame.inherited.clone()}else{tail};let bytes=frame.f.index.store.read_object(&reference).unwrap();let path=frame.f.index.store.object_path(&reference).unwrap();fs::remove_file(&path).unwrap();
        let clock=Clock::new();let cancelled=clock.clone();let called=Arc::new(AtomicBool::new(false));let marker=called.clone();
        let hook=crate::store::backed_retention::physical_failure_hook_for_test(Box::new(move||{marker.store(true,Ordering::Release);cancelled.0.cancel();}));
        let mut read=None;let result=owner.read_retaining(&mut read,reference,clock.clone());drop(hook);
        assert!(result.is_err());assert!(called.load(Ordering::Acquire));assert!(clock.remaining().is_err());assert!(read.as_ref().unwrap().verified_bytes().is_err());drop(read);
        fs::write(&path,&bytes).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
        let mut after=None;assert!(owner.read_retaining(&mut after,owner.root_data().clone(),Clock::new()).is_err());assert!(after.as_ref().unwrap().verified_bytes().is_err());drop(after);
        if inherited{let(clock,stage,root,_)=staged_child(&frame,b"fenced shared parent");let mut sibling=None;assert!(frame.f.index.store.publish_source_overlay_retaining(&mut sibling,stage.id,"fenced-sibling".into(),root,frame.parent.clone(),clock).is_err());}
        else{let sibling=publish_child(&frame,"healthy-sibling",b"shared parent remains healthy");assert!(sibling.acknowledgment().is_ok());}
    }
}

#[test]
fn a_replaced_local_namespace_fences_the_source_parent_even_if_old_names_are_restored(){
    let frame=source_fixture();let publication=publish_child(&frame,"namespace-child",b"original namespace");let owner=publication.acknowledgment().unwrap();
    let root=frame.f.index.store.root().to_path_buf();let retained=root.with_extension("retained-original");fs::rename(&root,&retained).unwrap();
    // This is a newly captured store, never custody for the original owners.
    let replacement=LocalStore::open(&root).unwrap();drop(replacement);let mut failed=None;
    assert!(owner.read_retaining(&mut failed,owner.root_data().clone(),Clock::new()).is_err());assert!(failed.as_ref().unwrap().verified_bytes().is_err());drop(failed);
    fs::remove_dir_all(&root).unwrap();fs::rename(&retained,&root).unwrap();let mut restored=None;
    assert!(owner.read_retaining(&mut restored,owner.root_data().clone(),Clock::new()).is_err());assert!(restored.as_ref().unwrap().verified_bytes().is_err());drop(restored);
    let(clock,stage,child,_)=staged_child(&frame,b"cannot clear observed namespace failure");let mut sibling=None;
    assert!(frame.f.index.store.publish_source_overlay_retaining(&mut sibling,stage.id,"namespace-sibling".into(),child,frame.parent.clone(),clock).is_err());
}


#[test]
fn invalid_initial_retention_is_absorbing_across_read_capture_publish_reattach_and_gc(){
    for operation in 0..5{let frame=source_fixture();let publication=publish_child(&frame,"initial-child",b"retained child");let owner=publication.acknowledgment().unwrap();
        let(clock,stage,root,_)=staged_child(&frame,b"cannot use repaired custody");let path=frame.f.index.store.root().join("retention.json");let bytes=fs::read(&path).unwrap();fs::write(&path,b"{invalid retention envelope").unwrap();
        match operation{
            0=>{let mut out=None;assert!(owner.read_retaining(&mut out,owner.root_data().clone(),Clock::new()).is_err());assert!(out.as_ref().unwrap().verified_bytes().is_err());drop(out);},
            1=>{let mut out=None;assert!(frame.base.derive_source_parent_retaining(&mut out,Clock::new()).is_err());assert!(out.as_ref().unwrap().binding_data().is_err());drop(out);},
            2=>{let mut out=None;assert!(frame.f.index.store.publish_source_overlay_retaining(&mut out,stage.id,"unacknowledged-child".into(),root,frame.parent.clone(),clock).is_err());assert!(out.as_ref().unwrap().acknowledgment().is_err());drop(out);},
            3=>{let mut out=None;assert!(frame.f.index.store.reattach_source_overlay_retaining(&mut out,"initial-child".into(),frame.parent.clone(),Clock::new()).is_err());assert!(out.as_ref().unwrap().reattached_owner().is_err());drop(out);},
            _=>{let mut out=None;assert!(frame.f.index.store.gc_source_overlays_retaining(&mut out,vec![frame.prep.acknowledgment().unwrap().retention().clone()],vec![frame.base.clone()],vec![owner.clone()],Duration::ZERO,Clock::new()).is_err());assert!(out.as_ref().unwrap().partial_report_data().is_none());drop(out);}
        }
        // Dropping failed carriers models process loss, never a retirement ACK.
        fs::write(&path,&bytes).unwrap();let mut after=None;assert!(owner.read_retaining(&mut after,owner.root_data().clone(),Clock::new()).is_err());assert!(after.as_ref().unwrap().verified_bytes().is_err());drop(after);
        let(clock,stage,root,_)=staged_child(&frame,b"shared parent absorbed initial failure");let mut sibling=None;
        assert!(frame.f.index.store.publish_source_overlay_retaining(&mut sibling,stage.id,"after-repair".into(),root,frame.parent.clone(),clock).is_err());assert!(sibling.as_ref().unwrap().acknowledgment().is_err());
    }
}

#[test]
fn missing_namespace_and_invalid_custody_file_observations_survive_exact_repairs(){
    for scenario in 0..5{let frame=source_fixture();let publication=publish_child(&frame,"custody-child",b"custody");let owner=publication.acknowledgment().unwrap();let root=frame.f.index.store.root();
        let path=root.join("retention.json");let bytes=fs::read(&path).unwrap();let moved=root.with_extension("temporarily-absent");
        match scenario{
            0=>fs::rename(root,&moved).unwrap(),1=>fs::remove_file(&path).unwrap(),2=>fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap(),
            3=>{let mut value:serde_json::Value=serde_json::from_slice(&bytes).unwrap();value["checksum"]=serde_json::Value::String("0".repeat(64));fs::write(&path,canonical_json(&value).unwrap()).unwrap();},
            _=>rewrite_state(&frame.f,|state|state.source_dependencies.as_mut().unwrap().pins.get_mut("custody-child").unwrap().depth=99)
        }
        let mut out=None;assert!(owner.read_retaining(&mut out,owner.root_data().clone(),Clock::new()).is_err());assert!(out.as_ref().unwrap().verified_bytes().is_err());drop(out);
        if scenario==0{fs::rename(&moved,root).unwrap();}else{fs::write(&path,&bytes).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o600)).unwrap();}
        let mut after=None;assert!(owner.read_retaining(&mut after,owner.root_data().clone(),Clock::new()).is_err());assert!(after.as_ref().unwrap().verified_bytes().is_err());
    }
}

#[test]
fn cancellation_at_initial_retention_observation_cannot_erase_shared_custody_failure(){
    let frame=source_fixture();let publication=publish_child(&frame,"initial-boundary",b"initial physical boundary");let owner=publication.acknowledgment().unwrap();
    let path=frame.f.index.store.root().join("retention.json");let bytes=fs::read(&path).unwrap();fs::remove_file(&path).unwrap();
    let clock=Clock::new();let cancelled=clock.clone();let called=Arc::new(AtomicBool::new(false));let marker=called.clone();
    let hook=crate::store::backed_retention::physical_failure_hook_for_test(Box::new(move||{marker.store(true,Ordering::Release);cancelled.0.cancel();}));
    let mut out=None;assert!(owner.read_retaining(&mut out,owner.root_data().clone(),clock.clone()).is_err());drop(hook);assert!(called.load(Ordering::Acquire));assert!(clock.remaining().is_err());assert!(out.as_ref().unwrap().verified_bytes().is_err());drop(out);
    fs::write(&path,&bytes).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o600)).unwrap();let mut after=None;
    assert!(owner.read_retaining(&mut after,owner.root_data().clone(),Clock::new()).is_err());assert!(after.as_ref().unwrap().verified_bytes().is_err());
}

#[test]
fn initial_resource_exhaustion_and_arbitrary_io_failure_remain_unknown_without_fencing(){
    for errno in [libc::EMFILE,libc::EIO]{let frame=source_fixture();let publication=publish_child(&frame,"unknown-initial",b"unknown IO outcome");let owner=publication.acknowledgment().unwrap();
        let called=Arc::new(AtomicBool::new(false));let marker=called.clone();let hook=crate::store::backed_retention::physical_failure_hook_for_test(Box::new(move||{marker.store(true,Ordering::Release);}));
        let mut out=None;assert!(owner.read_initial_open_error_retaining(&mut out,owner.root_data().clone(),Clock::new(),errno).is_err());assert!(out.as_ref().unwrap().verified_bytes().is_err());assert!(!called.load(Ordering::Acquire));drop(hook);drop(out);
        assert_eq!(read_child(owner,frame.inherited.clone()).unwrap(),frame.inherited_bytes);let sibling=publish_child(&frame,"after-unknown",b"fresh original operation");assert!(sibling.acknowledgment().is_ok());
    }
}

#[test]
fn retained_overlay_qualifies_build_input_and_serves_inherited_delta_and_foreign_bytes(){
    use crate::{workspace::retained_build_input::RetainedRunnableBuildInput,store::environment_dependencies::{RetainedRunnableInputs,RetainedRunnableRead}};
    let frame=source_fixture();let(clock,stage,root,tail)=staged_child(&frame,b"new retained input tail");let mut publication=None;
    frame.f.index.store.publish_source_overlay_retaining(&mut publication,stage.id,"retained-build-child".into(),root.clone(),frame.parent.clone(),clock).unwrap();let owner=publication.as_ref().unwrap().acknowledgment().unwrap().clone();
    let inputs=RetainedRunnableInputs::Overlay(owner);let before=frame.f.state();let source=crate::snapshot::decode_source(&frame.f.index.store.read_object(&frame.base_root.source).unwrap()).unwrap();
    let mut qualification=None;RetainedRunnableBuildInput::capture_retaining(&mut qualification,frame.f.index.store.clone(),inputs.clone(),source.projection_policy,Default::default(),Clock::new()).unwrap();
    let qualified=qualification.as_mut().unwrap();assert_eq!(qualified.input().unwrap().checkpoint(),&root);assert_eq!(qualified.input().unwrap().base_environment(),Some(frame.env.root_data().unwrap()));qualified.current_retaining(Clock::new()).unwrap();
    for(reference,bytes)in [(frame.inherited.clone(),frame.inherited_bytes.clone()),(tail,b"new retained input tail".to_vec()),(frame.f.payload.clone(),frame.f.payload_bytes.clone())]{let mut read=None;
        inputs.read_retaining(&mut read,reference,Clock::new()).unwrap();assert!(matches!(&read,Some(RetainedRunnableRead::Overlay(_))));assert_eq!(read.as_ref().unwrap().verified_bytes().unwrap(),bytes);}
    assert_eq!(frame.f.state(),before);assert!(!frame.f.index.store.object_path(&frame.f.payload).unwrap().exists());assert!(inputs.matches_record_data(&before,"retained-build-child"));assert!(!inputs.matches_record_data(&before,"source-base"));
    let mut altered=before.clone();altered.source_dependencies.as_mut().unwrap().pins.get_mut("retained-build-child").unwrap().delta_sha256="0".repeat(64);assert!(!inputs.matches_record_data(&altered,"retained-build-child"));
}


#[test]
fn original_flat_read_observing_invalid_shared_retention_fences_derived_overlay_after_repair(){
    let frame=source_fixture();let publication=publish_child(&frame,"flat-observer-child",b"same genuine flat anchor");let owner=publication.acknowledgment().unwrap();
    let path=frame.f.index.store.root().join("retention.json");let bytes=fs::read(&path).unwrap();fs::remove_file(&path).unwrap();let mut flat_read=None;
    assert!(frame.base.read_retaining(&mut flat_read,frame.inherited.clone(),Clock::new()).is_err());assert!(flat_read.as_ref().unwrap().verified_bytes().is_err());drop(flat_read);
    fs::write(&path,&bytes).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o600)).unwrap();let mut overlay_read=None;
    assert!(owner.read_retaining(&mut overlay_read,owner.root_data().clone(),Clock::new()).is_err());assert!(overlay_read.as_ref().unwrap().verified_bytes().is_err());drop(overlay_read);
    let(clock,stage,root,_)=staged_child(&frame,b"same anchor cannot clear observed failure");let mut sibling=None;
    assert!(frame.f.index.store.publish_source_overlay_retaining(&mut sibling,stage.id,"flat-observer-sibling".into(),root,frame.parent.clone(),clock).is_err());assert!(sibling.as_ref().unwrap().acknowledgment().is_err());
}
