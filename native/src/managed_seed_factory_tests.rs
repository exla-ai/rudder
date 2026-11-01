//! Actual local worker/handle/TLS regressions, UNRUN. No Root/Joined/fork proof
//! constructors, model calls, namespaces, sockets or source fixture authority.
use super::*;
use std::{cell::RefCell,sync::mpsc::{self,Receiver,SyncSender},thread,time::Duration};
fn original()->FinalizationBudget {FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
fn reserve(gate:&Arc<SeedAdmission>)->Arc<Mutex<SeedFactoryOwner>> {
    let mut out=None;SeedFactoryOwner::reserve_retaining(&mut out,gate,&original(),Arc::new(AtomicBool::new(false))).unwrap();Arc::new(Mutex::new(out.unwrap()))
}
struct Tls {entered:SyncSender<()>,release:Receiver<()>}
impl Drop for Tls {fn drop(&mut self){self.entered.send(()).unwrap();self.release.recv_timeout(Duration::from_secs(10)).unwrap();}}
thread_local! {static ORIGINAL_TLS:RefCell<Option<Tls>>=const {RefCell::new(None)};}
fn blocked(owner:&Arc<Mutex<SeedFactoryOwner>>)->(Receiver<()>,SyncSender<()>) {
    let (start,wait)=mpsc::sync_channel(1);let (entered,observed)=mpsc::sync_channel(1);let (release,closed)=mpsc::sync_channel(1);
    let actual=owner.clone();let mut worker=Some(thread::spawn(move||{
        wait.recv_timeout(Duration::from_secs(5)).unwrap();actual.lock().unwrap().check_original_thread().unwrap();
        ORIGINAL_TLS.with(|tls|*tls.borrow_mut()=Some(Tls {entered,release:closed}));
    }));
    owner.lock().unwrap().register_retaining(&mut worker).unwrap();assert!(worker.is_none());start.send(()).unwrap();(observed,release)
}
fn join(owner:&Arc<Mutex<SeedFactoryOwner>>) {
    let (driver,budget)=owner.lock().unwrap().join_driver().unwrap();driver.start_join_observer().unwrap();driver.await_joined(budget.remaining().unwrap()).unwrap();
    assert!(owner.lock().unwrap().poll_joined().unwrap());
}
#[test]
fn original_handle_and_thread_identity_are_required_before_any_factory_source_access() {
    let gate=SeedAdmission::new();let owner=reserve(&gate);let (step,receive)=mpsc::sync_channel(1);let (report,events)=mpsc::sync_channel(1);
    let actual=owner.clone();let mut worker=Some(thread::spawn(move||{
        report.send(actual.lock().unwrap().check_original_thread().is_err()).unwrap();receive.recv_timeout(Duration::from_secs(5)).unwrap();
        report.send(actual.lock().unwrap().check_original_thread().is_ok()).unwrap();
    }));
    assert!(events.recv_timeout(Duration::from_secs(5)).unwrap());owner.lock().unwrap().register_retaining(&mut worker).unwrap();
    assert!(owner.lock().unwrap().check_original_thread().is_err(),"registering parent cannot impersonate original worker");
    step.send(()).unwrap();assert!(events.recv_timeout(Duration::from_secs(5)).unwrap());join(&owner);
}
#[test]
fn two_returned_callbacks_with_live_tls_still_bound_admission_but_joined_live_owners_allow_a_third() {
    let gate=SeedAdmission::new();let first=reserve(&gate);let second=reserve(&gate);
    let (entered_a,release_a)=blocked(&first);let (entered_b,release_b)=blocked(&second);
    entered_a.recv_timeout(Duration::from_secs(5)).unwrap();entered_b.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!first.lock().unwrap().poll_joined().unwrap());assert!(!second.lock().unwrap().poll_joined().unwrap());
    let mut third=None;assert!(SeedFactoryOwner::reserve_retaining(&mut third,&gate,&original(),Arc::new(AtomicBool::new(false))).is_err());assert!(third.is_none());
    assert_eq!(gate.factories.load(Ordering::Acquire),2);
    release_a.send(()).unwrap();join(&first); // first owner remains LIVE/retained.
    let third=reserve(&gate);let (entered_c,release_c)=blocked(&third);entered_c.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(gate.factories.load(Ordering::Acquire),2);release_c.send(()).unwrap();join(&third);
    release_b.send(()).unwrap();join(&second);assert_eq!(gate.factories.load(Ordering::Acquire),0);
    assert!(first.lock().unwrap().is_joined()&&second.lock().unwrap().is_joined()&&third.lock().unwrap().is_joined());
}
#[test]
fn dropped_join_delivery_never_releases_capacity_before_actual_original_tls_join() {
    let gate=SeedAdmission::new();let owner=reserve(&gate);let (entered,release)=blocked(&owner);entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let (driver,budget)=owner.lock().unwrap().join_driver().unwrap();driver.start_join_observer().unwrap();drop(driver);
    assert_eq!(gate.factories.load(Ordering::Acquire),1);assert!(!owner.lock().unwrap().poll_joined().unwrap());
    release.send(()).unwrap();let (driver,_)=owner.lock().unwrap().join_driver().unwrap();drop(driver.await_joined(budget.remaining().unwrap()).unwrap());
    // Losing receipt delivery retains same cached physical result; explicit
    // adoption of that actual result releases only the original permit.
    assert_eq!(gate.factories.load(Ordering::Acquire),1);assert!(owner.lock().unwrap().poll_joined().unwrap());assert_eq!(gate.factories.load(Ordering::Acquire),0);
}
#[test]
fn actual_factory_panic_retains_original_join_failure_and_charged_owner() {
    let gate=SeedAdmission::new();let owner=reserve(&gate);let mut handle=Some(thread::spawn(||panic!("original factory failure")));
    owner.lock().unwrap().register_retaining(&mut handle).unwrap();let (driver,budget)=owner.lock().unwrap().join_driver().unwrap();
    assert!(driver.await_joined(budget.remaining().unwrap()).is_err());assert!(owner.lock().unwrap().poll_joined().is_err());
    assert_eq!(gate.factories.load(Ordering::Acquire),1);assert!(!owner.lock().unwrap().is_joined());assert!(owner.lock().unwrap().worker.is_some());
}
#[test]
fn original_cancellation_refuses_registration_and_keeps_actual_untransferred_handle() {
    let gate=SeedAdmission::new();let owner=reserve(&gate);let (release,wait)=mpsc::sync_channel(1);
    let mut handle=Some(thread::spawn(move||{wait.recv_timeout(Duration::from_secs(5)).unwrap();}));
    owner.lock().unwrap().cancelled.store(true,Ordering::Release);assert!(owner.lock().unwrap().register_retaining(&mut handle).is_err());
    assert!(handle.is_some()&&!owner.lock().unwrap().registration_present());
    owner.lock().unwrap().cancelled.store(false,Ordering::Release);assert!(owner.lock().unwrap().original.remaining().is_err(),"observed cancellation stays absorbing");
    release.send(()).unwrap();handle.take().unwrap().join().unwrap();assert_eq!(gate.factories.load(Ordering::Acquire),1);
}
#[test]
fn closing_admission_refuses_new_original_source_reads_without_claiming_worker_exit() {
    let gate=SeedAdmission::new();let owner=reserve(&gate);let (entered,release)=blocked(&owner);entered.recv_timeout(Duration::from_secs(5)).unwrap();
    gate.close();let mut new=None;assert!(SeedFactoryOwner::reserve_retaining(&mut new,&gate,&original(),Arc::new(AtomicBool::new(false))).is_err());
    assert!(!owner.lock().unwrap().is_joined());assert_eq!(gate.factories.load(Ordering::Acquire),1);
    release.send(()).unwrap();join(&owner);assert_eq!(gate.factories.load(Ordering::Acquire),0);
}
