//! Actual original thread/TLS custody. No Root client, helper ACK, floor drain,
//! source release or successful task completion is fabricated by these tests.
use super::*;
use std::{cell::RefCell,sync::{mpsc,Barrier,atomic::{AtomicUsize,Ordering}},thread,time::Duration};
struct TlsGate {entered:mpsc::SyncSender<()>,release:mpsc::Receiver<()>,dropped:Arc<AtomicUsize>}
impl Drop for TlsGate {fn drop(&mut self){let _=self.entered.send(());let _=self.release.recv_timeout(Duration::from_secs(5));self.dropped.fetch_add(1,Ordering::AcqRel);}}
thread_local! {static TLS_GATE:RefCell<Option<TlsGate>>=const {RefCell::new(None)};}
fn original(ledger:&RuntimeUseLedger)->RuntimePreparationUse {let mut out=None;ledger.preparation_retaining(&mut out).unwrap();out.unwrap()}
fn joined_entry(ledger:&RuntimeUseLedger)->bool {matches!(&ledger.state().unwrap().entries[0],Entry::Preparation{joined:Some(_),..})}

#[test]
fn returned_body_and_foreign_receipt_cannot_substitute_for_original_tls_join() {
    let ledger=RuntimeUseLedger::new();let guard=original(&ledger);
    let (entered,wait)=mpsc::sync_channel(1);let (release,released)=mpsc::sync_channel(1);
    let dropped=Arc::new(AtomicUsize::new(0));let actual_drop=dropped.clone();
    let worker=OwnedWorkerJoin::new(thread::spawn(move||TLS_GATE.with(|slot|*slot.borrow_mut()=Some(TlsGate{entered,release:released,dropped:actual_drop}))));
    guard.retain_original_worker(&worker).unwrap();worker.start_join_observer().unwrap();
    wait.recv_timeout(Duration::from_secs(2)).unwrap();
    let foreign=OwnedWorkerJoin::new(thread::spawn(||{}));let foreign_join=foreign.await_joined(Duration::from_secs(2)).unwrap();
    assert!(guard.retain_original_joined(&foreign_join).is_err());assert!(!joined_entry(&ledger));
    assert!(worker.try_joined().unwrap().is_none());assert_eq!(dropped.load(Ordering::Acquire),0);
    // Even an observer deadline has no authority to settle the blocked TLS.
    assert!(worker.await_joined(Duration::from_millis(10)).is_err());assert!(!joined_entry(&ledger));
    release.send(()).unwrap();let joined=worker.await_joined(Duration::from_secs(2)).unwrap();
    assert_eq!(dropped.load(Ordering::Acquire),1);
    assert!(guard.retain_original_joined(&foreign_join).is_err());assert!(!joined_entry(&ledger));
    guard.retain_original_joined(&joined).unwrap();guard.retain_original_joined(&joined).unwrap();
    assert!(joined_entry(&ledger));
    let state=ledger.state().unwrap();let Entry::Preparation{worker:Some(held),joined:Some(held_join)}=&state.entries[0]else{panic!("original owners absent")};
    assert!(held.try_joined().unwrap().unwrap().same_worker(&joined));assert!(held_join.same_worker(&joined));
}
#[test]
fn dropped_or_failed_spawn_reservation_never_reopens_admission_or_recycles_capacity() {
    let ledger=RuntimeUseLedger::new();drop(original(&ledger));let mut second=None;
    assert!(ledger.preparation_retaining(&mut second).is_err());assert!(second.is_none());
    let state=ledger.state().unwrap();assert_eq!(state.entries.len(),1);
    assert!(matches!(state.entries[0],Entry::Preparation{worker:None,joined:None}));
}
#[test]
fn closed_admission_is_absorbing_but_same_admitted_worker_can_finish() {
    let ledger=RuntimeUseLedger::new();let guard=original(&ledger);ledger.close().unwrap();ledger.close().unwrap();
    let worker=OwnedWorkerJoin::new(thread::spawn(||{}));guard.retain_original_worker(&worker).unwrap();
    let joined=worker.await_joined(Duration::from_secs(2)).unwrap();guard.retain_original_joined(&joined).unwrap();
    let mut next=None;assert!(ledger.preparation_retaining(&mut next).is_err());assert!(next.is_none());
    assert_eq!(ledger.state().unwrap().entries.len(),1);assert!(joined_entry(&ledger));
}
#[test]
fn replacement_worker_and_receipt_cannot_replace_same_original_registration() {
    let ledger=RuntimeUseLedger::new();let guard=original(&ledger);
    let original=OwnedWorkerJoin::new(thread::spawn(||{}));guard.retain_original_worker(&original).unwrap();
    let replacement=OwnedWorkerJoin::new(thread::spawn(||{}));let wrong=replacement.await_joined(Duration::from_secs(2)).unwrap();
    assert!(guard.retain_original_worker(&replacement).is_err());assert!(guard.retain_original_joined(&wrong).is_err());
    let actual=original.await_joined(Duration::from_secs(2)).unwrap();guard.retain_original_joined(&actual).unwrap();assert!(joined_entry(&ledger));
}
struct PanicCustody(Arc<AtomicUsize>);
impl Drop for PanicCustody{fn drop(&mut self){self.0.fetch_add(1,Ordering::AcqRel);}}
#[test]
fn actual_panicked_worker_remains_owned_and_never_becomes_preparation_joined() {
    let ledger=RuntimeUseLedger::new();let guard=original(&ledger);let dropped=Arc::new(AtomicUsize::new(0));let held=dropped.clone();
    let worker=OwnedWorkerJoin::new(thread::spawn(move||std::panic::panic_any(PanicCustody(held))));
    guard.retain_original_worker(&worker).unwrap();assert!(worker.await_joined(Duration::from_secs(2)).is_err());
    drop(worker);drop(guard);assert_eq!(dropped.load(Ordering::Acquire),0,"ledger retains actual original panic result");
    assert!(!joined_entry(&ledger));
    let state=ledger.state().unwrap();let Entry::Preparation{worker:Some(actual),joined:None}=&state.entries[0]else{panic!("failed original owner absent")};
    assert!(actual.try_joined().is_err());assert_eq!(state.entries.len(),1);
}
#[test]
fn close_and_reserve_have_one_linearization_point_and_no_post_close_admission() {
    for _ in 0..16 {
        let ledger=RuntimeUseLedger::new();let begin=Arc::new(Barrier::new(3));
        let acquire=ledger.clone();let first=begin.clone();
        let reservation=thread::spawn(move||{first.wait();let mut out=None;let result=acquire.preparation_retaining(&mut out);(result.is_ok(),out)});
        let close=ledger.clone();let second=begin.clone();let closer=thread::spawn(move||{second.wait();close.close().unwrap()});
        begin.wait();let (admitted,guard)=reservation.join().unwrap();closer.join().unwrap();
        let mut after=None;assert!(ledger.preparation_retaining(&mut after).is_err());assert!(after.is_none());
        let state=ledger.state().unwrap();assert!(state.closed);assert_eq!(state.entries.len(),usize::from(admitted));assert_eq!(guard.is_some(),admitted);
    }
}

#[test]
fn live_observation_retains_same_original_worker_through_held_tls_and_dropped_aliases() {
    let ledger=RuntimeUseLedger::new();let guard=original(&ledger);let observation=ledger.observation();
    let (entered,wait)=mpsc::sync_channel(1);let (release,released)=mpsc::sync_channel(1);
    let dropped=Arc::new(AtomicUsize::new(0));let actual_drop=dropped.clone();
    let worker=OwnedWorkerJoin::new(thread::spawn(move||TLS_GATE.with(|slot|*slot.borrow_mut()=Some(TlsGate {entered,release:released,dropped:actual_drop}))));
    guard.retain_original_worker(&worker).unwrap();worker.start_join_observer().unwrap();wait.recv_timeout(Duration::from_secs(2)).unwrap();
    let first=observation.try_sample().unwrap().unwrap();let data=first.data().unwrap();
    assert_eq!(data.preparation_uses,1);assert!(data.preparation_worker_registered&&!data.preparation_join_observed);
    drop(first);drop(guard);drop(worker);drop(ledger);
    let held=observation.try_sample().unwrap().unwrap();let actual=held.preparation.as_ref().unwrap().0.as_ref().unwrap().clone();
    assert!(actual.try_joined().unwrap().is_none());assert_eq!(dropped.load(Ordering::Acquire),0);
    let expired=rudder_engine::execution::finalization_budget::FinalizationBudget::begin(Duration::from_secs(1)).unwrap();expired.cancel();
    assert!(expired.remaining().is_err());assert!(!held.data().unwrap().preparation_join_observed);
    release.send(()).unwrap();let joined=actual.await_joined(Duration::from_secs(2)).unwrap();
    assert!(held.data().unwrap().preparation_join_observed);
    let next=observation.try_sample().unwrap().unwrap();
    assert!(next.preparation.as_ref().unwrap().0.as_ref().unwrap().try_joined().unwrap().unwrap().same_worker(&joined));
    assert!(next.data().unwrap().preparation_join_observed);assert_eq!(dropped.load(Ordering::Acquire),1);
    assert!(!next.data().unwrap().ledger_closed_at_sample);assert!(expired.remaining().is_err());
}

#[test]
fn actual_registration_racing_live_sample_is_not_a_final_set_or_admission_close() {
    for _ in 0..16 {
        let ledger=RuntimeUseLedger::new();let observation=ledger.observation();
        let before=observation.try_sample().unwrap().unwrap().data().unwrap();assert_eq!(before.preparation_uses,0);
        let begin=Arc::new(Barrier::new(2));let actual=ledger.clone();let ready=begin.clone();
        let producer=thread::spawn(move||{ready.wait();let guard=original(&actual);
            let worker=OwnedWorkerJoin::new(thread::spawn(||{}));guard.retain_original_worker(&worker).unwrap();(guard,worker)});
        begin.wait();let racing=observation.try_sample().unwrap();
        let (_guard,worker)=producer.join().unwrap();let joined=worker.await_joined(Duration::from_secs(2)).unwrap();
        if let Some(sample)=racing {assert!(sample.data().unwrap().preparation_uses<=1);assert!(!sample.closed_at_sample);}
        let after=observation.try_sample().unwrap().unwrap();assert_eq!(after.data().unwrap().preparation_uses,1);
        assert!(after.preparation.as_ref().unwrap().0.as_ref().unwrap().try_joined().unwrap().unwrap().same_worker(&joined));
        assert_eq!(before.preparation_uses,0,"old DATA never becomes a final census");assert!(!after.closed_at_sample);
        let mut replacement=None;assert!(ledger.preparation_retaining(&mut replacement).is_err());
        assert!(replacement.is_none());assert_eq!(ledger.state().unwrap().entries.len(),1);
    }
}

#[test]
fn observation_ledger_contention_and_poison_preserve_original_reservation() {
    let ledger=RuntimeUseLedger::new();let _guard=original(&ledger);let observation=ledger.observation();
    let held=ledger.state().unwrap();assert!(observation.try_sample().unwrap().is_none());drop(held);
    assert_eq!(observation.try_sample().unwrap().unwrap().data().unwrap().preparation_uses,1);
    let poison=ledger.clone();assert!(thread::spawn(move||{let _held=poison.state.lock().unwrap();panic!("actual original ledger poison");}).join().is_err());
    assert!(observation.try_sample().is_err());
    let held=observation.ledger.state.lock().unwrap_or_else(|p|p.into_inner());
    assert_eq!(held.entries.len(),1);assert!(!held.closed);
    assert!(matches!(held.entries[0],Entry::Preparation {worker:None,joined:None}));
}

#[test]
fn failed_original_preparation_sample_keeps_real_panic_without_join_observation() {
    let ledger=RuntimeUseLedger::new();let guard=original(&ledger);let observation=ledger.observation();
    let dropped=Arc::new(AtomicUsize::new(0));let actual=dropped.clone();
    let worker=OwnedWorkerJoin::new(thread::spawn(move||std::panic::panic_any(PanicCustody(actual))));
    guard.retain_original_worker(&worker).unwrap();assert!(worker.await_joined(Duration::from_secs(2)).is_err());
    drop(guard);drop(worker);drop(ledger);
    assert!(observation.try_sample().unwrap().unwrap().data().is_err());assert_eq!(dropped.load(Ordering::Acquire),0);
    assert_eq!(observation.ledger.state().unwrap().entries.len(),1);
}
