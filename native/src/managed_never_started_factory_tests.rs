// Actual original factory thread identity and a real cancelled clock. No Root,
// seed, App preparation, fork or helper receipts are constructed here.
#[test]
fn late_factory_outcome_can_be_retained_after_original_cancel_without_restoring_source_access() {
    let gate=SeedAdmission::new();let original=FinalizationBudget::begin(Duration::from_secs(20)).unwrap();
    let cancelled=Arc::new(AtomicBool::new(false));let mut owner=None;
    SeedFactoryOwner::reserve_retaining(&mut owner,&gate,&original,cancelled.clone()).unwrap();
    let owner=Arc::new(Mutex::new(owner.unwrap()));let actual=owner.clone();
    let (release,wait)=std::sync::mpsc::sync_channel(1);let (report,result)=std::sync::mpsc::sync_channel(1);
    let mut worker=Some(std::thread::spawn(move||{
        wait.recv_timeout(Duration::from_secs(5)).unwrap();let held=actual.lock().unwrap();
        report.send((held.check_original_result_thread().is_ok(),held.check_original_thread().is_err())).unwrap();
    }));
    owner.lock().unwrap().register_retaining(&mut worker).unwrap();
    cancelled.store(true,Ordering::Release);original.cancel();gate.close();
    assert!(owner.lock().unwrap().check_original_result_thread().is_err(),"parent cannot impersonate actual factory even on cleanup");
    release.send(()).unwrap();assert_eq!(result.recv_timeout(Duration::from_secs(5)).unwrap(),(true,true));
    let (driver,_)=owner.lock().unwrap().join_driver().unwrap();driver.await_joined(Duration::from_secs(5)).unwrap();
    assert!(owner.lock().unwrap().poll_joined().unwrap());assert_eq!(gate.factories.load(Ordering::Acquire),0);
    assert!(original.remaining().is_err(),"ownership retention must not renew original work clock");
}
