//! Actual same-UID filesystem/CAS DATA fixtures. The cfg(test) baked capture
//! does not create Root/config/SDK/project/mount authority. ALL tests unrun by
//! the author; Root owns compilation and execution.
use super::*;
use crate::{content::{ContentStore,FileNode,FILE_LAYOUT},directory::{DirectoryEntry,EntryNode,PagedDirectory},
    execution::finalization_budget::FinalizationBudget,store::baked_image::{BakedImageCas,BakedImageInventoryData,BakedImageRoots,BAKED_IMAGE_LAYOUT}};
use std::{os::fd::OwnedFd,sync::{Weak,atomic::{AtomicBool,Ordering}},time::Instant};
struct Clock {budget:FinalizationBudget,cancelled:Arc<AtomicBool>}
impl Clock {fn new()->Arc<Self>{Self::lasting(Duration::from_secs(30))}fn lasting(duration:Duration)->Arc<Self>{Arc::new(Self{budget:FinalizationBudget::begin(duration).unwrap(),cancelled:Arc::new(AtomicBool::new(false))})}}
impl ImageImportFence for Clock {fn remaining(&self)->Result<Duration>{if self.cancelled.load(Ordering::Acquire){self.budget.cancel();}self.budget.remaining()}}
struct Fixture {temporary:tempfile::TempDir,root:PathBuf,local:LocalStore,baked:Arc<BakedImageCas>,installer:ObjectRef,manifest:ObjectRef,payload:ObjectRef,file:ObjectRef,bytes:Vec<u8>}
impl Fixture {
    fn new()->Self {
        let temporary=if Path::new("/workspace").is_dir(){tempfile::tempdir_in("/workspace").unwrap()}else{tempfile::tempdir().unwrap()};let root=temporary.path().join("baked");fs::create_dir(&root).unwrap();
        let backing=LocalStore::open(root.join("cas")).unwrap();let mut import=None;LocalImageImport::stage_retaining(&mut import,backing.clone(),Clock::new()).unwrap();let import=import.unwrap();
        let bytes=vec![b'X';4*1024*1024];let payload=ContentStore::put_blob(&import,&bytes).unwrap();let tail=ContentStore::put_blob(&import,b"actual-tail").unwrap();
        let file=ContentStore::put_file_node(&import,&FileNode{format_version:1,layout:FILE_LAYOUT.into(),level:0,logical_size:payload.size_bytes+tail.size_bytes,children:vec![payload.clone(),tail.clone()]}).unwrap();
        let topology=ContentStore::put_blob(&import,b"{\"format_version\":1,\"groups\":[]}").unwrap();
        let installer=PagedDirectory::new(&import).build(0o555,vec![DirectoryEntry::new(b"hardlinks.json",EntryNode::File{object:topology.clone(),logical_size:topology.size_bytes,mode:0o444}).unwrap(),
            DirectoryEntry::new(b"tool",EntryNode::File{object:file.clone(),logical_size:payload.size_bytes+tail.size_bytes,mode:0o555}).unwrap()]).unwrap();
        let mut whole=Sha256::new();whole.update(&bytes);whole.update(b"actual-tail");let raw=canonical_json(&serde_json::json!({"format_version":1,"installer_input":installer,"files":[{"path":"/fixture/tool","sha256":hex::encode(whole.finalize()),"size_bytes":payload.size_bytes+tail.size_bytes,"content":file}]})).unwrap();
        let manifest=ObjectRef::from_bytes(ObjectKind::Blob,&raw);import.receive_manifest(&manifest,&raw).unwrap();let ack=import.commit_pins("fixture-installer",&installer,"fixture-manifest",&manifest).unwrap();let objects=import.objects_after_commit(&ack).unwrap();let retained=backing.retention().unwrap();
        let roots=BakedImageRoots{format_version:1,manifest:manifest.clone(),installer_input:installer.clone()};let raw_retention=fs::read(root.join("cas/retention.json")).unwrap();
        let inventory=BakedImageInventoryData{format_version:1,layout:BAKED_IMAGE_LAYOUT.into(),roots:roots.clone(),objects:objects.clone(),total_bytes:objects.iter().map(|r|r.size_bytes).sum(),retention:ObjectRef::from_bytes(ObjectKind::Blob,&raw_retention),retention_generation:retained.generation,pins:retained.pins};let raw_inventory=canonical_json(&inventory).unwrap();let expected=ObjectRef::from_bytes(ObjectKind::Blob,&raw_inventory);
        fs::write(root.join("inventory.json"),raw_inventory).unwrap();fs::write(root.join("pin.json"),canonical_json(&roots).unwrap()).unwrap();fs::write(root.join("manifest.json"),raw).unwrap();drop(import);
        for p in ["inventory.json","pin.json","manifest.json","cas/lock","cas/retention.json"]{fs::set_permissions(root.join(p),fs::Permissions::from_mode(0o444)).unwrap();}
        for shard in fs::read_dir(root.join("cas/objects")).unwrap(){fs::set_permissions(shard.unwrap().path(),fs::Permissions::from_mode(0o555)).unwrap();}
        for p in ["cas/objects","cas/tmp","cas",""]{fs::set_permissions(root.join(p),fs::Permissions::from_mode(0o555)).unwrap();}
        let root_fd:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&root).unwrap().into();
        let inventory_fd:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(root.join("inventory.json")).unwrap().into();
        let mut baked=None;BakedImageCas::capture_local_fixture_retaining(&mut baked,root_fd,inventory_fd,expected,Clock::new()).unwrap();let local=LocalStore::open(temporary.path().join("local")).unwrap();
        Self{temporary,root,local,baked:baked.unwrap(),installer,manifest,payload,file,bytes}
    }
    fn inputs(&self)->Arc<VerifiedBakedInputs>{let mut out=None;VerifiedBakedInputs::capture_retaining(&mut out,self.baked.clone(),Clock::new()).unwrap();out.unwrap()}
    fn environment(&self)->(StagingLease,ObjectRef){let stage=self.local.begin_staging(Duration::from_secs(120)).unwrap();let empty=self.local.put_directory(DirectoryManifest{format_version:1,mode:0o755,entries:Vec::new()}).unwrap();
        let network=self.local.put_blob(b"actual per-VM input fixture DATA").unwrap();let root=self.local.put_metadata(ObjectKind::EnvironmentRoot,&crate::snapshot::EnvironmentV2{format_version:2,directory:empty,platform:BTreeMap::from([("os".into(),"linux".into()),("abi".into(),"fixture-data".into())]),installer_inputs:vec![self.installer.clone(),self.manifest.clone(),network],provenance:BTreeMap::from([("test_scope".into(),"storage-data-not-Root".into())])}).unwrap();(stage,root)}
    fn publication(&self,pin:&str)->(BackedPublication,Arc<VerifiedBakedInputs>,ObjectRef){let source=self.inputs();let(stage,root)=self.environment();
        (BackedPublication{io:StoreIo::staged(self.local.clone(),Clock::new()),sources:vec![source.clone()],staging_id:stage.id,pin:pin.into(),root:root.clone(),operation:Uuid::new_v4().to_string(),status:BackedPublicationStatus::Captured,record:None,ack:None,failure:None,version_before:Vec::new(),version_after:Vec::new(),fault:None},source,root)}
    fn publish(&self,pin:&str)->Arc<BackedPinOwner>{let(mut owner,_,_)=self.publication(pin);owner.run().unwrap();owner.acknowledgment().unwrap().clone()}
    fn raw_state(&self)->RetentionState{let bytes=fs::read(self.local.root.join("retention.json")).unwrap();let e:StateEnvelope=serde_json::from_slice(&bytes).unwrap();assert_eq!(ObjectRef::from_bytes(ObjectKind::Blob,&canonical_json(&e.state).unwrap()).digest,e.checksum);e.state}
    fn baked_path(&self,r:&ObjectRef)->PathBuf{self.root.join("cas/objects").join(&r.digest[..2]).join(&r.digest[2..])}
}
impl Drop for Fixture {fn drop(&mut self){fn writable(path:&Path){let Ok(m)=fs::symlink_metadata(path)else{return;};if !m.is_dir()||m.file_type().is_symlink(){return}let _=fs::set_permissions(path,fs::Permissions::from_mode(0o700));if let Ok(entries)=fs::read_dir(path){for e in entries.flatten(){writable(&e.path())}}}writable(&self.root);}}

#[test]
fn backed_inputs_actual_paged_file_graph_validates_without_local_payload_or_pins(){let f=Fixture::new();let source=f.inputs();let b=source.binding_data().unwrap();assert_eq!(b.installer_input,f.installer);assert!(source.contains_data(&f.payload));assert!(source.contains_data(&f.manifest));
    assert!(fs::read_dir(f.local.root.join("objects")).unwrap().next().is_none());assert!(f.local.retention().unwrap().pins.is_empty());let mut read=None;source.read_retaining(&mut read,f.payload.clone(),Clock::new()).unwrap();assert_eq!(read.as_ref().unwrap().verified_bytes().unwrap(),f.bytes);}

#[test]
fn backed_real_publication_has_exact_local_foreign_partition_and_content_only_environment(){let f=Fixture::new();let owner=f.publish("env");let state=f.local.retention().unwrap();assert!(state.pins.is_empty());let data=state.backed_retention.as_ref().unwrap();assert!(data.holds.is_empty());assert_eq!(data.bindings.len(),1);assert_eq!(data.pins["env"],owner.record);
    assert!(!f.local.object_path(&f.payload).unwrap().exists());assert!(!owner.record.local_objects.contains(&f.payload));assert_eq!(owner.origin_data(),BackedOwnerOrigin::OriginalPublication);
    let mut read=None;owner.read_retaining(&mut read,owner.root_data().clone(),Clock::new()).unwrap();let env=crate::snapshot::decode_environment(read.as_ref().unwrap().verified_bytes().unwrap()).unwrap();assert!(env.installer_inputs.contains(&f.installer));assert!(env.installer_inputs.contains(&f.manifest));
    let mut bytes=None;owner.read_retaining(&mut bytes,f.payload.clone(),Clock::new()).unwrap();assert_eq!(bytes.as_ref().unwrap().verified_bytes().unwrap(),f.bytes);}

#[test]
fn backed_ordinary_gc_and_unpin_refuse_without_deleting_or_releasing(){let f=Fixture::new();let owner=f.publish("env");let garbage=f.local.put_blob(b"must remain").unwrap();assert!(f.local.gc(Duration::ZERO).is_err());assert!(f.local.unpin("env").is_err());assert!(f.local.object_path(&garbage).unwrap().exists());assert_eq!(&f.raw_state().backed_retention.unwrap().pins["env"],owner.record_data());}

#[test]
fn backed_gc_requires_every_actual_owner_and_retains_siblings(){let f=Fixture::new();let one=f.publish("one");let two=f.publish("two");let garbage=f.local.put_blob(b"unused").unwrap();let mut refused=None;
    assert!(f.local.gc_backed_retaining(&mut refused,vec![one.clone()],Duration::ZERO,Clock::new()).is_err());assert_eq!(refused.as_ref().unwrap().partial_report_data().unwrap().removed_objects,0);assert!(f.local.object_path(&garbage).unwrap().exists());drop(refused);
    let mut good=None;f.local.gc_backed_retaining(&mut good,vec![one.clone(),two.clone()],Duration::ZERO,Clock::new()).unwrap();assert!(good.as_ref().unwrap().acknowledgment().unwrap().removed_objects>=1);assert!(!f.local.object_path(&garbage).unwrap().exists());
    for r in one.record.local_objects.iter().chain(&two.record.local_objects){assert!(f.local.object_path(r).unwrap().exists());}}

#[test]
fn backed_reopen_data_alone_refuses_gc_then_explicit_actual_reattachment_revalidates(){let f=Fixture::new();let original=f.publish("env");let root=original.root_data().clone();drop(original);let reopened=LocalStore::open(f.local.root()).unwrap();let garbage=reopened.put_blob(b"restart orphan").unwrap();let mut missing=None;
    assert!(reopened.gc_backed_retaining(&mut missing,Vec::new(),Duration::ZERO,Clock::new()).is_err());assert!(reopened.object_path(&garbage).unwrap().exists());drop(missing);
    let mut invalid=None;assert!(reopened.reattach_backed_retaining(&mut invalid,"env".into(),Vec::new(),Clock::new()).is_err());assert!(invalid.as_ref().unwrap().reattached_owner().is_err());drop(invalid);
    let mut reattached=None;reopened.reattach_backed_retaining(&mut reattached,"env".into(),vec![f.inputs()],Clock::new()).unwrap();let owner=reattached.as_ref().unwrap().reattached_owner().unwrap().clone();assert_eq!(owner.origin_data(),BackedOwnerOrigin::ExplicitReattachment);assert_eq!(owner.root_data(),&root);
    let mut gc=None;reopened.gc_backed_retaining(&mut gc,vec![owner],Duration::ZERO,Clock::new()).unwrap();assert!(!reopened.object_path(&garbage).unwrap().exists());}

#[test]
fn backed_partial_hold_blocks_restart_reattachment_and_gc(){let f=Fixture::new();let(mut publication,source,_)=f.publication("unknown");publication.fault=Some(TestPoint::AfterHold);assert!(publication.run().is_err());assert!(publication.acknowledgment().is_err());assert!(publication.io.gc.is_some());let raw=f.raw_state();assert_eq!(raw.backed_retention.unwrap().holds.len(),1);drop(publication);
    let mut reattach=None;assert!(f.local.reattach_backed_retaining(&mut reattach,"unknown".into(),vec![source],Clock::new()).is_err());assert!(reattach.as_ref().unwrap().reattached_owner().is_err());drop(reattach);
    let mut gc=None;assert!(f.local.gc_backed_retaining(&mut gc,Vec::new(),Duration::ZERO,Clock::new()).is_err());assert_eq!(gc.as_ref().unwrap().partial_report_data().unwrap().removed_objects,0);}

#[test]
fn backed_known_commit_lost_ack_and_directory_sync_failure_never_readback_promote(){for fault in [TestPoint::FinalRename,TestPoint::AfterCommit]{let f=Fixture::new();let(mut publication,_,_)=f.publication("unknown");publication.fault=Some(fault);assert!(publication.run().is_err());assert!(publication.acknowledgment().is_err());assert!(publication.io.gc.is_some());assert!(f.raw_state().backed_retention.unwrap().pins.contains_key("unknown"));assert!(publication.run().is_err());
    if fault==TestPoint::FinalRename{assert!(publication.io.pending.is_some());assert!(publication.io.pending_name.is_some());} }}

#[test]
fn backed_current_writers_keep_envelope_ordinary_pins_and_object_sets(){let f=Fixture::new();let owner=f.publish("env");let ordinary=f.local.put_blob(b"ordinary retained").unwrap();f.local.pin("ordinary",&ordinary).unwrap();let explicit=f.local.put_blob(b"explicit retained").unwrap();let stage=f.local.begin_staging(Duration::from_secs(30)).unwrap();let receipt=f.local.retain_object_set(&stage.id,"explicit",&[explicit.clone()]).unwrap();let garbage=f.local.put_blob(b"unused").unwrap();let before=f.local.retention().unwrap();assert_eq!(before.format_version,2);assert!(before.backed_retention.is_some());
    let mut gc=None;f.local.gc_backed_retaining(&mut gc,vec![owner],Duration::ZERO,Clock::new()).unwrap();assert!(f.local.object_path(&ordinary).unwrap().exists());assert!(f.local.object_path(&explicit).unwrap().exists());assert!(!f.local.object_path(&garbage).unwrap().exists());assert_eq!(f.local.object_set_receipt("explicit").unwrap(),Some(receipt));assert_eq!(f.local.retention().unwrap().backed_retention,before.backed_retention);}

#[test]
fn backed_image_import_writer_preserves_original_backed_and_other_pins(){let f=Fixture::new();let owner=f.publish("env");let mut import=None;LocalImageImport::stage_retaining(&mut import,f.local.clone(),Clock::new()).unwrap();let import=import.unwrap();let root=PagedDirectory::new(&import).build(0o555,Vec::new()).unwrap();let raw=b"new immutable image DATA";let manifest=ObjectRef::from_bytes(ObjectKind::Blob,raw);import.receive_manifest(&manifest,raw).unwrap();import.commit_pins("other-installer",&root,"other-manifest",&manifest).unwrap();
    let state=f.local.retention().unwrap();assert!(state.pins.contains_key("other-installer"));assert_eq!(&state.backed_retention.unwrap().pins["env"],owner.record_data());}

#[test]
fn backed_present_local_corruption_never_falls_back_to_valid_foreign(){let f=Fixture::new();let owner=f.publish("env");f.local.receive(&f.payload,&f.bytes).unwrap();let p=f.local.object_path(&f.payload).unwrap();fs::set_permissions(&p,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&p,vec![b'Y';f.bytes.len()]).unwrap();fs::set_permissions(&p,fs::Permissions::from_mode(0o444)).unwrap();let mut read=None;
    assert!(owner.read_retaining(&mut read,f.payload.clone(),Clock::new()).is_err());let read=read.unwrap();assert!(read.io.pending.is_some()&&!read.io.partial.is_empty());assert!(read.foreign.is_none());assert!(read.verified_bytes().is_err());}

#[test]
fn backed_foreign_corruption_is_retained_and_observed_before_gc_deletion(){let f=Fixture::new();let owner=f.publish("env");let garbage=f.local.put_blob(b"preserve after corrupt").unwrap();let path=f.baked_path(&f.payload);fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'Z';f.bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();let mut read=None;
    assert!(owner.read_retaining(&mut read,f.payload.clone(),Clock::new()).is_err());assert!(read.as_ref().unwrap().foreign.is_some());assert!(read.as_ref().unwrap().verified_bytes().is_err());drop(read);
    let mut gc=None;assert!(f.local.gc_backed_retaining(&mut gc,vec![owner],Duration::ZERO,Clock::new()).is_err());assert!(f.local.object_path(&garbage).unwrap().exists());}

#[test]
fn backed_cancelled_constructor_and_read_hold_actual_outputs_without_poisoning_other_job(){let f=Fixture::new();let clock=Clock::new();clock.cancelled.store(true,Ordering::Release);let mut input=None;
    assert!(VerifiedBakedInputs::capture_retaining(&mut input,f.baked.clone(),clock).is_err());assert!(input.is_some());assert!(input.as_ref().unwrap().binding_data().is_err());let owner=f.publish("env");let weak:Weak<BackedPinOwner>=Arc::downgrade(&owner);
    let cancelled=Clock::new();cancelled.cancelled.store(true,Ordering::Release);let mut read=None;assert!(owner.read_retaining(&mut read,f.payload.clone(),cancelled).is_err());drop(owner);assert!(weak.upgrade().is_some());assert!(read.as_ref().unwrap().verified_bytes().is_err());
    let retained=weak.upgrade().unwrap();let mut good=None;retained.read_retaining(&mut good,f.payload.clone(),Clock::new()).unwrap();assert_eq!(good.as_ref().unwrap().verified_bytes().unwrap(),f.bytes);}

#[test]
fn backed_actual_gc_wait_uses_same_original_clock_and_keeps_real_lock_fd(){let f=Fixture::new();let(mut publication,_,_)=f.publication("wait");let lock=OpenOptions::new().read(true).write(true).open(f.local.root.join("lock")).unwrap();FileExt::lock_exclusive(&lock).unwrap();publication.io.original=Clock::lasting(Duration::from_millis(30));let start=Instant::now();assert!(publication.run().is_err());assert!(start.elapsed()<Duration::from_secs(2));assert!(publication.io.gc.is_some()&&!publication.io.locked);assert!(publication.acknowledgment().is_err());}

#[test]
fn backed_replacement_store_and_stale_binding_refuse_gc_before_unlink(){let f=Fixture::new();let owner=f.publish("env");let path=f.local.root.clone();let moved=f.temporary.path().join("moved");fs::rename(&path,&moved).unwrap();let replacement=LocalStore::open(&path).unwrap();let garbage=replacement.put_blob(b"replacement stays").unwrap();let mut gc=None;
    assert!(replacement.gc_backed_retaining(&mut gc,vec![owner],Duration::ZERO,Clock::new()).is_err());assert!(replacement.object_path(&garbage).unwrap().exists());}

#[test]
fn backed_typed_child_or_inventory_mutation_refuses_original_graph(){let f=Fixture::new();let path=f.baked_path(&f.file);fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();let mut raw=fs::read(&path).unwrap();raw[0]^=1;fs::write(&path,raw).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();let mut source=None;
    assert!(VerifiedBakedInputs::capture_retaining(&mut source,f.baked.clone(),Clock::new()).is_err());assert!(source.as_ref().unwrap().binding_data().is_err());assert!(source.as_ref().unwrap().retained_capture_read_data());}

#[test]
fn backed_new_local_parent_must_match_actual_foreign_filenode_summary(){let f=Fixture::new();let source=f.inputs();let stage=f.local.begin_staging(Duration::from_secs(120)).unwrap();let directories=crate::directory::LocalDirectoryStore::new(&f.local,&stage.id);
    let root=PagedDirectory::new(&directories).build(0o755,vec![DirectoryEntry::new(b"wrong",EntryNode::File{object:f.file.clone(),logical_size:f.payload.size_bytes+1,mode:0o555}).unwrap()]).unwrap();
    let mut out=None;assert!(f.local.publish_backed_retaining(&mut out,stage.id,"bad-parent".into(),root,vec![source],Clock::new()).is_err());assert!(out.as_ref().unwrap().acknowledgment().is_err());assert!(out.as_ref().unwrap().io.gc.is_some());assert_eq!(f.raw_state().backed_retention.unwrap().holds.len(),1);}

#[test]
fn backed_unknown_child_cannot_be_skipped_into_foreign_partition(){let f=Fixture::new();let source=f.inputs();let stage=f.local.begin_staging(Duration::from_secs(120)).unwrap();let absent=ObjectRef::from_bytes(ObjectKind::Blob,b"never installed in either store");
    let root=f.local.put_metadata(ObjectKind::ActionResult,&ReferencedMetadata{format_version:1,references:vec![f.installer.clone(),absent],payload:serde_json::json!({"scope":"actual fixture incomplete DATA"})}).unwrap();
    let mut out=None;assert!(f.local.publish_backed_retaining(&mut out,stage.id,"missing".into(),root,vec![source],Clock::new()).is_err());assert!(out.as_ref().unwrap().acknowledgment().is_err());assert_eq!(f.raw_state().backed_retention.unwrap().pins.len(),0);}

#[test]
fn backed_old_deny_unknown_reader_refuses_and_data_version_is_not_a_live_owner(){let f=Fixture::new();let owner=f.publish("env");let state=f.raw_state();
    #[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Old{format_version:u32,generation:u64,pins:BTreeMap<String,ObjectRef>,staging:BTreeMap<String,StagingLease>,#[serde(default)]image_import_holds:BTreeMap<String,ImageImportHold>}
    assert!(serde_json::from_slice::<Old>(&canonical_json(&state).unwrap()).is_err());let mut bad=state;bad.backed_retention.as_mut().unwrap().format_version=99;assert!(backed_data::validate_retention(&bad).is_err());
    let raw=canonical_json(owner.record_data()).unwrap();let _:BackedPinData=serde_json::from_slice(&raw).unwrap();assert!(f.local.gc(Duration::ZERO).is_err());}


#[test]
fn backed_replicated_foreign_object_survives_reopen_without_repartitioning(){
    let f=Fixture::new();let original=f.publish("env");let record=original.record_data().clone();
    f.local.receive(&f.payload,&f.bytes).unwrap();f.local.pin("replica",&f.payload).unwrap();
    let garbage=f.local.put_blob(b"replication unrelated garbage").unwrap();drop(original);
    let reopened=LocalStore::open(f.local.root()).unwrap();let mut reattached=None;
    reopened.reattach_backed_retaining(&mut reattached,"env".into(),vec![f.inputs()],Clock::new()).unwrap();
    let owner=reattached.as_ref().unwrap().reattached_owner().unwrap().clone();
    assert_eq!(owner.record_data(),&record);assert!(!owner.record_data().local_objects.contains(&f.payload));
    let mut gc=None;reopened.gc_backed_retaining(&mut gc,vec![owner],Duration::ZERO,Clock::new()).unwrap();
    assert!(reopened.object_path(&f.payload).unwrap().exists());assert!(!reopened.object_path(&garbage).unwrap().exists());
    assert_eq!(reopened.retention().unwrap().pins.get("replica"),Some(&f.payload));
}

#[test]
fn backed_replica_cannot_hide_missing_or_corrupt_original_backing_during_reattach(){
    for missing in [false,true]{let f=Fixture::new();let original=f.publish("env");let source=f.inputs();
        f.local.receive(&f.payload,&f.bytes).unwrap();f.local.pin("replica",&f.payload).unwrap();drop(original);
        let path=f.baked_path(&f.payload);
        if missing{fs::set_permissions(path.parent().unwrap(),fs::Permissions::from_mode(0o755)).unwrap();fs::remove_file(&path).unwrap();}
        else{fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'Z';f.bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();}
        let reopened=LocalStore::open(f.local.root()).unwrap();let mut reattached=None;
        assert!(reopened.reattach_backed_retaining(&mut reattached,"env".into(),vec![source],Clock::new()).is_err());
        assert!(reattached.as_ref().unwrap().reattached_owner().is_err());assert!(reopened.object_path(&f.payload).unwrap().exists());
    }
}

#[test]
fn backed_corrupt_local_replica_cannot_hide_behind_intact_original_backing_on_reopen(){
    let f=Fixture::new();let original=f.publish("env");f.local.receive(&f.payload,&f.bytes).unwrap();
    let path=f.local.object_path(&f.payload).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&path,vec![b'Z';f.bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();drop(original);
    let reopened=LocalStore::open(f.local.root()).unwrap();let mut reattached=None;
    assert!(reopened.reattach_backed_retaining(&mut reattached,"env".into(),vec![f.inputs()],Clock::new()).is_err());
    assert!(reattached.as_ref().unwrap().io.pending.is_some());assert!(reattached.as_ref().unwrap().reattached_owner().is_err());
}

#[test]
fn backed_gc_replica_does_not_discharge_corrupt_foreign_obligation(){
    let f=Fixture::new();let owner=f.publish("env");f.local.receive(&f.payload,&f.bytes).unwrap();f.local.pin("replica",&f.payload).unwrap();
    let garbage=f.local.put_blob(b"must remain until original backing verified").unwrap();let path=f.baked_path(&f.payload);
    fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();fs::write(&path,vec![b'Z';f.bytes.len()]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    let mut gc=None;assert!(f.local.gc_backed_retaining(&mut gc,vec![owner],Duration::ZERO,Clock::new()).is_err());
    assert!(gc.as_ref().unwrap().io.foreign.is_some());assert!(gc.as_ref().unwrap().acknowledgment().is_err());
    assert_eq!(gc.as_ref().unwrap().partial_report_data().unwrap().removed_objects,0);assert!(f.local.object_path(&garbage).unwrap().exists());
}

#[path="backed_io_fd_settlement_tests.rs"]mod io_fd_settlement;
