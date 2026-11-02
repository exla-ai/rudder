//! Focused production gate + actual original worker/cell/TLS regressions.
//! The harness is NOT a synthetic WorkerCarrier, Root client/ACK or task
//! completion. Genuine machine bootstrap/seed fixtures are still required for
//! full MachineControl/SeedTicket snapshot and App-continuation qualification.
use super::*;
use std::{cell::RefCell,sync::{Barrier,mpsc,atomic::AtomicUsize},thread};

fn campaign()->Arc<CoveredCampaign> {
    Arc::new(CoveredCampaign {original:FinalizationBudget::begin(Duration::from_secs(3)).unwrap(),cancelled:Arc::new(AtomicBool::new(false))})
}
struct ReservationHarness {
    admission:CoveredTaskGenerationAdmission,
    originals:Vec<Arc<Cell<OwnedWorkerJoin>>>,
}
impl ReservationHarness {
    fn new()->Self{Self{admission:CoveredTaskGenerationAdmission::new(),originals:Vec::new()}}
    fn close(&mut self,campaign:&Arc<CoveredCampaign>)->Vec<Arc<Cell<OwnedWorkerJoin>>> {
        self.admission.retain_campaign(campaign).unwrap();self.admission.close(campaign).unwrap();self.originals.clone()
    }
}
struct HeldTls {entered:mpsc::SyncSender<()>,release:mpsc::Receiver<()>,dropped:Arc<AtomicUsize>}
impl Drop for HeldTls {
    fn drop(&mut self){let _=self.entered.send(());let _=self.release.recv_timeout(Duration::from_secs(5));self.dropped.fetch_add(1,Ordering::AcqRel);}
}
thread_local! {static HELD_TLS:RefCell<Option<HeldTls>>=const {RefCell::new(None)};}

#[test]
fn covered_close_retains_same_cells_and_original_tls_can_finish_after_closure() {
    let mut registry=ReservationHarness::new();registry.admission.require_task_reservation_open().unwrap();
    let (entered,wait)=mpsc::sync_channel(1);let (release,released)=mpsc::sync_channel(1);
    let dropped=Arc::new(AtomicUsize::new(0));let actual_drop=dropped.clone();
    let actual=Cell::new(OwnedWorkerJoin::new(thread::spawn(move||HELD_TLS.with(|slot|*slot.borrow_mut()=Some(HeldTls{entered,release:released,dropped:actual_drop})))));
    registry.originals.push(actual.clone());let cleanup=campaign();let pinned=registry.close(&cleanup);
    assert!(Arc::ptr_eq(&actual,&pinned[0]));registry.originals.clear();drop(actual);
    let worker=pinned[0].owner.lock().unwrap().clone();worker.start_join_observer().unwrap();
    wait.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(worker.try_joined().unwrap().is_none());assert_eq!(dropped.load(Ordering::Acquire),0);
    assert!(registry.admission.require_task_reservation_open().is_err());assert!(registry.admission.generation_discovery_closed());
    assert!(worker.await_joined(Duration::from_millis(10)).is_err());
    release.send(()).unwrap();let joined=worker.await_joined(Duration::from_secs(2)).unwrap();
    let same=pinned[0].owner.lock().unwrap().try_joined().unwrap().unwrap();assert!(same.same_worker(&joined));
    assert_eq!(dropped.load(Ordering::Acquire),1);assert!(registry.admission.require_task_reservation_open().is_err());
}

#[test]
fn original_cleanup_campaign_is_absorbing_and_cancellation_cannot_install_a_fresh_clock() {
    let mut gate=CoveredTaskGenerationAdmission::new();let original=campaign();let foreign=campaign();
    gate.retain_campaign(&original).unwrap();
    assert!(gate.require_task_reservation_open().is_ok(),"retaining a campaign alone does not claim closure");
    assert!(gate.retain_campaign(&foreign).is_err());assert!(gate.close(&foreign).is_err());
    gate.close(&original).unwrap();original.original.cancel();
    gate.retain_campaign(&original).unwrap();gate.close(&original).unwrap();
    assert!(original.original.remaining().is_err());assert!(gate.retain_campaign(&foreign).is_err());
    assert!(Arc::ptr_eq(gate.campaign.as_ref().unwrap(),&original));
    assert!(gate.require_task_reservation_open().is_err());assert!(gate.generation_discovery_closed());
}

#[test]
fn competing_close_and_reservation_pin_every_admitted_actual_worker_under_one_mutex() {
    for _ in 0..16 {
        let registry=Arc::new(Mutex::new(ReservationHarness::new()));let barrier=Arc::new(Barrier::new(3));let cleanup=campaign();
        let producer_registry=registry.clone();let start=barrier.clone();
        let producer=thread::spawn(move||{
            // Actual worker is a fixture owned before the gate race; no spawn,
            // callback or IO occurs while the registration mutex is held.
            let actual=Cell::new(OwnedWorkerJoin::new(thread::spawn(||{})));
            start.wait();let mut registry=producer_registry.lock().unwrap();
            let admitted=registry.admission.require_task_reservation_open().is_ok();
            if admitted{registry.originals.push(actual.clone());}(actual,admitted)});
        let closer_registry=registry.clone();let start=barrier.clone();let original=cleanup.clone();
        let closer=thread::spawn(move||{start.wait();closer_registry.lock().unwrap().close(&original)});
        barrier.wait();let (actual,admitted)=producer.join().unwrap();let pinned=closer.join().unwrap();
        assert_eq!(pinned.len(),usize::from(admitted));
        let joined=actual.owner.lock().unwrap().await_joined(Duration::from_secs(2)).unwrap();
        if admitted {
            assert!(Arc::ptr_eq(&actual,&pinned[0]));
            assert!(pinned[0].owner.lock().unwrap().try_joined().unwrap().unwrap().same_worker(&joined));
        }
        let mut registry=registry.lock().unwrap();assert!(registry.admission.require_task_reservation_open().is_err());
        assert!(registry.admission.generation_discovery_closed());
        let again=registry.close(&cleanup);assert_eq!(again.len(),pinned.len());
        for (first,second) in pinned.iter().zip(again.iter()){assert!(Arc::ptr_eq(first,second));}
    }
}

#[test]
fn dropping_external_campaign_alias_does_not_reopen_or_retain_a_cycle() {
    // The exact production gate owns only the same campaign. It never stores
    // an external drain owner/Shared or a per-poll history of captured cells.
    let mut gate=CoveredTaskGenerationAdmission::new();let original=campaign();let weak=Arc::downgrade(&original);
    gate.retain_campaign(&original).unwrap();gate.close(&original).unwrap();drop(original);
    let retained=weak.upgrade().unwrap();assert!(gate.retain_campaign(&campaign()).is_err());
    assert!(Arc::ptr_eq(gate.campaign.as_ref().unwrap(),&retained));drop(retained);drop(gate);assert!(weak.upgrade().is_none());
}
