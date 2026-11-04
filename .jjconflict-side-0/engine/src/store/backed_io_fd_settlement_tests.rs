//! Real CAS/publication/GC-descriptor cases. No RLIMIT changes, process/Root
//! grants, saved-pin ACKs, or owner reconstruction. Definitions UNRUN.
use super::*;
use std::sync::atomic::AtomicUsize;

fn shared_references(f:&Fixture)->Vec<ObjectRef>{
    let mut references=vec![f.installer.clone(),f.manifest.clone()];let mut shards=BTreeSet::new();
    for ordinal in 0..4096{
        let bytes=format!("shared actual publication payload {ordinal}").into_bytes();
        let reference=ObjectRef::from_bytes(ObjectKind::Blob,&bytes);
        if shards.insert(reference.digest[..2].to_owned()){
            f.local.receive(&reference,&bytes).unwrap();references.push(reference);
        }
        if shards.len()==64{break;}
    }
    assert_eq!(shards.len(),64);references
}
fn publication(f:&Fixture,references:&[ObjectRef],source:Arc<VerifiedBakedInputs>,ordinal:usize)->BackedPublication{
    let clock=Clock::new();let stage=f.local.begin_staging(clock.remaining().unwrap()).unwrap();
    let root=f.local.put_metadata(ObjectKind::ActionResult,&ReferencedMetadata{format_version:1,
        references:references.to_vec(),payload:serde_json::json!({"component":"actual retained IO settlement","ordinal":ordinal})}).unwrap();
    BackedPublication{io:StoreIo::staged(f.local.clone(),clock),sources:vec![source],staging_id:stage.id,
        pin:format!("settled:{ordinal}"),root,operation:Uuid::new_v4().to_string(),status:BackedPublicationStatus::Captured,
        record:None,ack:None,failure:None,version_before:Vec::new(),version_after:Vec::new(),fault:None}
}
fn assert_custody(owner:&BackedPinOwner){
    for(file,expected)in owner.custody.files.iter().zip(&owner.custody.ids){assert_eq!(&identity(file).unwrap(),expected);}
}

#[test]
fn fifty_known_publications_keep_actual_pins_and_custody_without_per_publication_shard_fds(){
    let f=Fixture::new();let source=f.inputs();let references=shared_references(&f);let mut held=Vec::new();
    for ordinal in 0..50{
        let mut original=publication(&f,&references,source.clone(),ordinal);original.run().unwrap();
        assert!(original.io.shards.is_empty(),"completed IO still retains one FD per visited shard");
        assert!(!original.io.locked&&original.io.gc.is_none());
        assert!(original.io.root.is_some()&&original.io.objects.is_some()&&original.io.tmp.is_some());
        let owner=original.acknowledgment().unwrap();assert_custody(owner);
        assert!(Arc::ptr_eq(&owner.sources[0],&source));
        assert!(references.iter().all(|r|owner.members.contains(r)));
        held.push(original); // Keep ALL original carriers/ACKs, not just the last.
    }
    let state=f.raw_state();let pins=&state.backed_retention.as_ref().unwrap().pins;
    assert_eq!(pins.len(),50);assert!(state.backed_retention.as_ref().unwrap().holds.is_empty());
    assert!(state.pins.is_empty());assert!(!f.local.object_path(&f.payload).unwrap().exists());
    for original in &held{
        let owner=original.acknowledgment().unwrap();assert_eq!(pins.get(&owner.pin),Some(&owner.record));assert_custody(owner);
        let mut read=None;owner.read_retaining(&mut read,owner.root_data().clone(),Clock::new()).unwrap();
        let read=read.unwrap();assert!(read.io.shards.is_empty());
        owner.root_data().verify(read.verified_bytes().unwrap()).unwrap();
    }
}

#[test]
fn unknown_directory_sync_or_late_publication_keeps_shards_gc_and_original_intents(){
    for fault in [TestPoint::FinalRename,TestPoint::AfterCommit]{
        let f=Fixture::new();let source=f.inputs();let references=shared_references(&f);
        let mut original=publication(&f,&references,source,0);original.fault=Some(fault);
        assert!(original.run().is_err());assert!(original.acknowledgment().is_err());
        assert!(original.io.locked&&original.io.gc.is_some());assert!(original.io.shards.len()>=64);
        assert!(original.io.root.is_some()&&original.io.objects.is_some()&&original.io.tmp.is_some());
        assert!(original.io.intent.is_some());assert!(original.failure().is_some());
        assert!(f.raw_state().backed_retention.unwrap().pins.contains_key("settled:0"));
        if fault==TestPoint::FinalRename{
            let before=original.io.shards.len();assert!(original.io.pending.is_some()&&original.io.pending_name.is_some());
            assert!(original.io.release_known().is_err());assert!(original.io.locked);
            assert_eq!(original.io.shards.len(),before);assert!(original.io.gc.is_some());
        }
        assert!(original.run().is_err());assert!(original.acknowledgment().is_err());
    }
}

#[test]
fn actual_failed_object_read_cannot_release_its_shard_or_gc_owner(){
    let f=Fixture::new();let reference=f.local.put_blob(b"actual local pending read").unwrap();
    let path=f.local.object_path(&reference).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&path,vec![b'!';reference.size_bytes as usize]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();
    let mut io=StoreIo::staged(f.local.clone(),Clock::new());io.acquire(true).unwrap();
    assert!(io.read_object(&reference,4*1024*1024).is_err());assert!(io.pending.is_some());assert!(!io.partial.is_empty());
    assert_eq!(io.shards.len(),1);assert!(io.release_known().is_err());
    assert!(io.locked&&io.gc.is_some()&&io.pending.is_some());assert_eq!(io.shards.len(),1);
}

struct CancelDuringSettlement{clock:Arc<Clock>,armed:AtomicBool,calls:AtomicUsize,cancel_at:usize}
impl ImageImportFence for CancelDuringSettlement{
    fn remaining(&self)->Result<Duration>{
        if self.armed.load(Ordering::Acquire)&&self.calls.fetch_add(1,Ordering::AcqRel)==self.cancel_at{
            self.clock.cancelled.store(true,Ordering::Release);
        }
        self.clock.remaining()
    }
}
#[test]
fn original_cancellation_before_or_after_unlock_retains_all_temporary_shards(){
    for cancel_at in [0,1]{
        let f=Fixture::new();let reference=f.local.put_blob(b"known synchronous bytes").unwrap();
        let clock=Clock::new();let constraint=clock.budget.constraint();
        let fence=Arc::new(CancelDuringSettlement{clock:clock.clone(),armed:AtomicBool::new(false),calls:AtomicUsize::new(0),cancel_at});
        let mut io=StoreIo::staged(f.local.clone(),fence.clone());io.acquire(true).unwrap();
        assert_eq!(io.read_object(&reference,4*1024*1024).unwrap().unwrap(),b"known synchronous bytes");
        let shard_fds=io.shards.iter().map(|(name,(file,_))|(name.clone(),file.as_raw_fd())).collect::<BTreeMap<_,_>>();
        fence.armed.store(true,Ordering::Release);assert!(io.release_known().is_err());
        assert_eq!(clock.budget.constraint(),constraint);assert!(io.gc.is_some());
        assert_eq!(io.locked,cancel_at==0);
        assert_eq!(io.shards.iter().map(|(name,(file,_))|(name.clone(),file.as_raw_fd())).collect::<BTreeMap<_,_>>(),shard_fds);
        for(file,expected)in io.shards.values(){assert_eq!(&directory(file).unwrap(),expected);}
    }
}

#[test]
fn settled_shard_cache_does_not_replace_actual_pin_custody_on_later_read(){
    let f=Fixture::new();let source=f.inputs();let references=shared_references(&f);
    let mut original=publication(&f,&references,source,0);original.run().unwrap();assert!(original.io.shards.is_empty());
    let owner=original.acknowledgment().unwrap().clone();let objects=owner.custody.ids[1].clone();
    let actual=f.local.root.join("objects");fs::rename(&actual,f.local.root.join("original-objects")).unwrap();fs::create_dir(&actual).unwrap();
    assert_eq!(identity(&owner.custody.files[1]).unwrap(),objects);
    let mut read=None;assert!(owner.read_retaining(&mut read,owner.root_data().clone(),Clock::new()).is_err());
    assert!(read.as_ref().unwrap().verified_bytes().is_err());assert!(read.as_ref().unwrap().io.gc.is_some());assert_custody(&owner);
}
