//! Original cache worker custody only. These fixtures do not issue a Root
//! enrollment, runtime stop ACK, seed retirement or whole-machine receipt.
use super::*;
use std::{cell::RefCell,sync::mpsc,thread};

struct HeldTls {entered:mpsc::SyncSender<()>,release:mpsc::Receiver<()>}
impl Drop for HeldTls {
    fn drop(&mut self){let _=self.entered.send(());let _=self.release.recv_timeout(Duration::from_secs(5));}
}
thread_local! {static HELD_TLS:RefCell<Option<HeldTls>>=const {RefCell::new(None)};}

fn original_cache()->(SeedCacheTicket,Arc<SeedAdmission>) {
    let original=FinalizationBudget::begin(Duration::from_secs(10)).unwrap();
    let admission=SeedAdmission::new();let mut factory=None;
    SeedFactoryOwner::reserve_retaining(&mut factory,&admission,&original,Arc::new(AtomicBool::new(false))).unwrap();
    (SeedCacheTicket {id:"cache".into(),seed_id:"seed".into(),seed:Weak::new(),original,app_thread:thread::current().id(),
        cell:Cell::new(CacheScope {factory:factory.unwrap(),guard:None,cache:None,weak:None,attempted:false,
            acknowledged:false,value:None,close:None,started:false})},admission)
}

#[test]
fn census_cache_without_original_registration_stays_pending() {
    let (ticket,admission)=original_cache();admission.close();
    assert!(!thread::spawn(move||ticket.ingress_poll_factory_joined().unwrap()).join().unwrap());
}

#[test]
fn census_cache_join_waits_for_original_tls_and_retains_original_expired_clock() {
    let (ticket,admission)=original_cache();
    let (entered,waiting)=mpsc::sync_channel(1);let (release,released)=mpsc::sync_channel(1);
    let mut worker=Some(thread::spawn(move||HELD_TLS.with(|slot|*slot.borrow_mut()=Some(HeldTls{entered,release:released}))));
    ticket.cell.owner.lock().unwrap().factory.register_retaining(&mut worker).unwrap();assert!(worker.is_none());
    let (driver,original)=ticket.cell.owner.lock().unwrap().factory.join_driver().unwrap();
    let retained=ticket.clone();
    assert!(!thread::spawn(move||retained.ingress_poll_factory_joined().unwrap()).join().unwrap());
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    admission.close();original.cancel();assert!(original.remaining().is_err());
    assert!(driver.try_joined().unwrap().is_none());
    release.send(()).unwrap();let actual=driver.await_joined(Duration::from_secs(2)).unwrap();
    let retained=ticket.clone();assert!(thread::spawn(move||retained.ingress_poll_factory_joined().unwrap()).join().unwrap());
    let held=ticket.cell.owner.lock().unwrap();assert!(held.factory.is_joined());
    let (same,clock)=held.factory.join_driver().unwrap();assert!(same.try_joined().unwrap().unwrap().same_worker(&actual));
    assert_eq!(clock.constraint(),original.constraint());assert!(clock.remaining().is_err());
    assert!(admission.check_open().is_err());
}

#[test]
fn census_cache_worker_panic_retains_same_failed_owner() {
    let (ticket,_admission)=original_cache();
    let mut worker=Some(thread::spawn(||panic!("original cache fixture failure")));
    ticket.cell.owner.lock().unwrap().factory.register_retaining(&mut worker).unwrap();
    let (driver,_)=ticket.cell.owner.lock().unwrap().factory.join_driver().unwrap();
    assert!(driver.await_joined(Duration::from_secs(2)).is_err());
    let retained=ticket.clone();assert!(thread::spawn(move||retained.ingress_poll_factory_joined().is_err()).join().unwrap());
    let held=ticket.cell.owner.lock().unwrap();assert!(held.factory.registration_present());assert!(!held.factory.is_joined());
    assert!(held.factory.join_driver().unwrap().0.try_joined().is_err());
}
