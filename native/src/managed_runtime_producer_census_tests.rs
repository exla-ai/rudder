//! Actual original threads and TLS, without manufacturing machine/Root owners.
use super::*;
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use std::{cell::RefCell,sync::{Arc,mpsc},thread,time::Duration};

struct HeldTls {entered:mpsc::SyncSender<()>,release:mpsc::Receiver<()>}
impl Drop for HeldTls {
    fn drop(&mut self){let _=self.entered.send(());let _=self.release.recv_timeout(Duration::from_secs(5));}
}
thread_local! {static TLS:RefCell<Option<HeldTls>>=const{RefCell::new(None)};}
fn original_worker()->(OwnedWorkerJoin,mpsc::Receiver<()>,mpsc::SyncSender<()>) {
    let (entered,waiting)=mpsc::sync_channel(1);let (release,released)=mpsc::sync_channel(1);
    let actual=OwnedWorkerJoin::new(thread::spawn(move||TLS.with(|slot|*slot.borrow_mut()=Some(HeldTls{entered,release:released}))));
    (actual,waiting,release)
}

#[test]
fn census_observes_original_tls_after_expiry_without_renewing_qualification_or_consuming_join() {
    let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();
    let (actual,waiting,release)=original_worker();let registration=Mutex::new(Some(actual.clone()));
    let first=worker(&registration).unwrap().unwrap();waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(first,WorkerCensus{slots:1,registered:1,joined:0});
    original.cancel();assert!(original.remaining().is_err());
    assert_eq!(worker(&registration).unwrap().unwrap().joined,0);
    release.send(()).unwrap();let joined=actual.await_joined(Duration::from_secs(2)).unwrap();
    assert_eq!(worker(&registration).unwrap().unwrap(),WorkerCensus{slots:1,registered:1,joined:1});
    let retained=registration.lock().unwrap();assert!(retained.as_ref().unwrap().try_joined().unwrap().unwrap().same_worker(&joined));
    assert!(original.remaining().is_err());
}

#[test]
fn census_distinguishes_unregistered_borrowed_and_panicked_original_workers() {
    let registration=Arc::new(Mutex::new(None));
    assert_eq!(worker(&registration).unwrap().unwrap(),WorkerCensus{slots:1,registered:0,joined:0});
    let held=registration.lock().unwrap();assert!(worker(&registration).unwrap().is_none());drop(held);
    let actual=OwnedWorkerJoin::new(thread::spawn(||panic!("original producer panic")));
    assert!(actual.await_joined(Duration::from_secs(2)).is_err());
    *registration.lock().unwrap()=Some(actual.clone());
    assert!(worker(&registration).is_err());assert!(registration.lock().unwrap().as_ref().unwrap().try_joined().is_err());
    let poisoned=registration.clone();assert!(thread::spawn(move||{let _held=poisoned.lock().unwrap();panic!("original registration poison");}).join().is_err());
    assert!(worker(&registration).is_err());
}

#[test]
fn census_parent_join_does_not_hide_live_importer_or_unregistered_coordinator() {
    let parent=OwnedWorkerJoin::new(thread::spawn(||{}));let _parent_joined=parent.await_joined(Duration::from_secs(2)).unwrap();
    let (importer,waiting,release)=original_worker();
    let mut data=RuntimeProducerCensusData {
        fresh_coordinators:WorkerCensus::observe(Some(&parent)).unwrap(),
        genesis_importers:WorkerCensus::observe(Some(&importer)).unwrap(),
        ..RuntimeProducerCensusData::default()
    };
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(!data.fresh_discovery_quiescent());
    release.send(()).unwrap();let _importer_joined=importer.await_joined(Duration::from_secs(2)).unwrap();
    data.genesis_importers=WorkerCensus::observe(Some(&importer)).unwrap();assert!(data.fresh_discovery_quiescent());
    data.pending_fresh_attempts=1;assert!(!data.fresh_discovery_quiescent());data.pending_fresh_attempts=0;
    data.fresh_coordinators=WorkerCensus::observe(None).unwrap();assert!(!data.fresh_discovery_quiescent());
}
