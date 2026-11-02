//! Real original factory/worker/TLS preflight only. These tests do NOT create
//! a whole task completion, Root client/ACK, Published acceptance or unused App
//! capsule. Those positive integration fixture gaps remain unqualified.
use super::*;
use crate::managed_seed_fork::{SeedAdmission,SeedFactoryOwner};
use std::{cell::RefCell,sync::mpsc,thread};

struct HeldFactoryTls {entered:mpsc::SyncSender<()>,release:mpsc::Receiver<()>}
impl Drop for HeldFactoryTls {
    fn drop(&mut self){let _=self.entered.send(());let _=self.release.recv_timeout(Duration::from_secs(5));}
}
thread_local! {static HELD_FACTORY_TLS:RefCell<Option<HeldFactoryTls>>=const {RefCell::new(None)};}

#[test]
fn factory_observation_requires_original_tls_and_never_mutates_cached_factory_or_clock() {
    let gate=SeedAdmission::new();let original=FinalizationBudget::begin(Duration::from_secs(10)).unwrap();
    let cancelled=Arc::new(AtomicBool::new(false));let mut factory=None;
    SeedFactoryOwner::reserve_retaining(&mut factory,&gate,&original,cancelled.clone()).unwrap();let mut factory=factory.unwrap();
    let (entered,wait)=mpsc::sync_channel(1);let (release,released)=mpsc::sync_channel(1);
    let mut child=Some(thread::spawn(move||HELD_FACTORY_TLS.with(|slot|*slot.borrow_mut()=Some(HeldFactoryTls {entered,release:released}))));
    factory.register_retaining(&mut child).unwrap();assert!(child.is_none());
    let (actual,held_original)=factory.join_driver().unwrap();actual.start_join_observer().unwrap();
    wait.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(!seeds::original_factory_join_observed(&factory).unwrap());assert!(!factory.is_joined());
    gate.close();cancelled.store(true,Ordering::Release);original.cancel();
    assert!(!seeds::original_factory_join_observed(&factory).unwrap());assert!(original.remaining().is_err());
    release.send(()).unwrap();let joined=actual.await_joined(Duration::from_secs(2)).unwrap();
    assert!(seeds::original_factory_join_observed(&factory).unwrap());assert!(!factory.is_joined(),"observer must not call mutating poll_joined");
    let (same,cached_original)=factory.join_driver().unwrap();assert!(same.try_joined().unwrap().unwrap().same_worker(&joined));
    assert_eq!(cached_original.constraint(),held_original.constraint());assert!(cached_original.remaining().is_err());
    assert!(gate.check_open().is_err());
}

#[test]
fn missing_registration_and_actual_factory_panic_remain_unobserved_without_replacement() {
    let gate=SeedAdmission::new();let original=FinalizationBudget::begin(Duration::from_secs(10)).unwrap();let mut factory=None;
    SeedFactoryOwner::reserve_retaining(&mut factory,&gate,&original,Arc::new(AtomicBool::new(false))).unwrap();let mut factory=factory.unwrap();
    assert!(!seeds::original_factory_join_observed(&factory).unwrap());assert!(!factory.registration_present());
    let mut worker=Some(thread::spawn(||panic!("actual retained factory panic")));factory.register_retaining(&mut worker).unwrap();
    let (actual,_)=factory.join_driver().unwrap();assert!(actual.await_joined(Duration::from_secs(2)).is_err());
    assert!(seeds::original_factory_join_observed(&factory).is_err());assert!(!factory.is_joined());
    let (same,_)=factory.join_driver().unwrap();assert!(same.try_joined().is_err());assert!(factory.registration_present());
}
