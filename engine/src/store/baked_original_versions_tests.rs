use super::*;
#[test]fn original_version_sweep_checks_complete_catalog_without_payload_reads(){let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let mut out=None;
    source.check_original_versions_retaining(&mut out,Clock::new()).unwrap();let sweep=out.as_ref().unwrap();assert!(sweep.acknowledged_data());
    assert_eq!(sweep.checked_objects_data(),source.inventory_data().unwrap().objects.len());assert_eq!(source.active.load(Ordering::Acquire),0);assert_eq!(source.bytes.load(Ordering::Acquire),0);}
#[test]fn original_version_sweep_refuses_in_place_same_length_mutation_after_capture(){let f=Fixture::new(b"manifest".to_vec());let source=f.capture();f.rewrite_file(&f.object_path(&f.payload),&vec![b'Y';128*1024]);let mut out=None;
    assert!(source.check_original_versions_retaining(&mut out,Clock::new()).is_err());assert!(!out.as_ref().unwrap().acknowledged_data());assert!(out.as_ref().unwrap().failure().is_some());assert!(source.state.lock().unwrap().failed);}
#[test]fn restoring_payload_and_mode_does_not_restore_original_version_authority(){let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let path=f.object_path(&f.payload);
    f.rewrite_file(&path,&vec![b'Y';128*1024]);f.rewrite_file(&path,&vec![b'X';128*1024]);let mut read=None;
    assert!(source.read_object_retaining(&mut read,f.payload.clone(),Clock::new()).is_err());assert!(read.as_ref().unwrap().file.is_some());assert!(read.as_ref().unwrap().verified_bytes().is_err());}
#[test]fn original_catalog_replacement_and_missing_leaf_refuse_before_payload_access(){for missing in [false,true]{let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let path=f.object_path(&f.payload);let shard=path.parent().unwrap();
    fs::set_permissions(shard,fs::Permissions::from_mode(0o755)).unwrap();fs::remove_file(&path).unwrap();if !missing{fs::write(&path,vec![b'X';128*1024]).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o444)).unwrap();}fs::set_permissions(shard,fs::Permissions::from_mode(0o555)).unwrap();
    let mut out=None;assert!(source.check_original_versions_retaining(&mut out,Clock::new()).is_err());assert!(!out.as_ref().unwrap().acknowledged_data());}}
#[test]fn cancelled_original_sweep_retains_source_without_poisoning_another_job(){let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let weak=Arc::downgrade(&source);let clock=Clock::new();clock.cancelled.store(true,Ordering::Release);let mut out=None;
    assert!(source.check_original_versions_retaining(&mut out,clock).is_err());drop(source);assert!(weak.upgrade().is_some());let actual=weak.upgrade().unwrap();assert!(!actual.state.lock().unwrap().failed);
    let mut next=None;actual.check_original_versions_retaining(&mut next,Clock::new()).unwrap();assert!(next.as_ref().unwrap().acknowledged_data());}
#[test]fn missing_original_leaf_during_capture_keeps_partial_catalog_and_refuses_ready(){let f=Fixture::new(b"manifest".to_vec());let path=f.object_path(&f.payload);let shard=path.parent().unwrap();fs::set_permissions(shard,fs::Permissions::from_mode(0o755)).unwrap();fs::remove_file(&path).unwrap();fs::set_permissions(shard,fs::Permissions::from_mode(0o555)).unwrap();
    let(root,inventory)=f.fds();let mut out=None;assert!(BakedImageCas::capture_owned(&mut out,root,inventory,f.catalog.clone(),Clock::new(),f.owner).is_err());let actual=out.as_ref().unwrap();assert!(actual.inventory_data().is_err());assert!(actual.state.lock().unwrap().failed);}

#[test]fn physical_sweep_mismatch_survives_cancellation_at_error_observation(){
    let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let original=Clock::new();
    *source.cancel_after_physical_failure.lock().unwrap()=Some(original.cancelled.clone());
    f.rewrite_file(&f.object_path(&f.payload),&vec![b'Y';128*1024]);let mut sweep=None;
    assert!(source.check_original_versions_retaining(&mut sweep,original.clone()).is_err());
    assert!(original.cancelled.load(Ordering::Acquire),"actual physical failure hook must cancel the SAME clock");
    assert!(source.physical_failed.load(Ordering::Acquire));assert!(source.state.lock().unwrap().failed);
    assert!(sweep.as_ref().unwrap().retains_pending_file_data());assert!(sweep.as_ref().unwrap().failure().is_some());
    assert!(source.assert_current(&*Clock::new()).is_err());assert!(source.inventory_data().is_err());
    let mut other=None;assert!(source.read_manifest_retaining(&mut other,Clock::new()).is_err());
    assert!(other.is_none(),"a fenced source refuses before staging a fresh manifest read");
}

#[test]fn physical_read_version_mismatch_survives_cancellation_at_error_observation(){
    let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let original=Clock::new();
    *source.cancel_after_physical_failure.lock().unwrap()=Some(original.cancelled.clone());
    f.rewrite_file(&f.object_path(&f.payload),&vec![b'Y';128*1024]);let mut read=None;
    assert!(source.read_object_retaining(&mut read,f.payload.clone(),original.clone()).is_err());
    assert!(original.cancelled.load(Ordering::Acquire));assert!(source.physical_failed.load(Ordering::Acquire));
    assert!(source.state.lock().unwrap().failed);assert!(read.as_ref().unwrap().file.is_some());assert!(read.as_ref().unwrap().verified_bytes().is_err());
    assert!(source.assert_current(&*Clock::new()).is_err());let mut other=None;
    assert!(source.read_manifest_retaining(&mut other,Clock::new()).is_err());
}

#[test]fn pure_mid_read_cancellation_does_not_become_physical_source_failure(){
    let f=Fixture::new(b"manifest".to_vec());let source=f.capture();let original=Clock::new();
    let mut retained=Some(BakedImageRead::stage(source.clone(),f.payload.clone(),false,original.clone()));
    retained.as_mut().unwrap().cancel_after_first_chunk=Some(original.cancelled.clone());
    assert!(retained.as_mut().unwrap().run().is_err());assert!(original.cancelled.load(Ordering::Acquire));
    assert!(!source.physical_failed.load(Ordering::Acquire));assert!(!source.state.lock().unwrap().failed);
    assert!(retained.as_ref().unwrap().file.is_some());assert!(retained.as_ref().unwrap().verified_bytes().is_err());
    source.assert_current(&*Clock::new()).unwrap();let mut other=None;
    source.read_manifest_retaining(&mut other,Clock::new()).unwrap();assert_eq!(other.as_ref().unwrap().verified_bytes().unwrap(),b"manifest");
}
