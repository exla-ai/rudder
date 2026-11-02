//! Actual production registry/role algorithms and real retained worker/TLS.
//! These generic roots do not fabricate a MachineControl, ingress, bootstrap,
//! Root ACK, environment or task issuer. Those integration positives are gaps.
use super::*;
use std::{cell::RefCell,sync::{Barrier,mpsc},thread};
use rudder_engine::execution::handoff::OwnedWorkerJoin;

struct Boundary<R>{closed:bool,slots:RootSlots<R>}
impl<R> Boundary<R>{fn close(&mut self)->Vec<Arc<R>>{self.closed=true;self.slots.roots.clone()}}

#[test]
fn original_root_registration_races_close_under_same_mutex() {
    for limit in [1,COMMAND_ROOTS]{for _ in 0..16{
        let boundary=Arc::new(Mutex::new(Boundary{closed:false,slots:RootSlots::new(limit)}));
        let barrier=Arc::new(Barrier::new(3));let owner=Arc::new(AtomicBool::new(false));
        let root=Arc::new(Root{actual:OriginalOwnerWeak::capture(&owner),children:OnceLock::<Arc<OriginalRole<AtomicBool>>>::new()});
        let reserve=boundary.clone();let selected=root.clone();let start=barrier.clone();
        let producer=thread::spawn(move||{start.wait();let mut b=reserve.lock().unwrap();let closed=b.closed;b.slots.insert(closed,selected).is_ok()});
        let close=boundary.clone();let start=barrier.clone();let closer=thread::spawn(move||{start.wait();close.lock().unwrap().close()});
        barrier.wait();let admitted=producer.join().unwrap();let snapshot=closer.join().unwrap();
        assert_eq!(snapshot.len(),usize::from(admitted));if admitted{assert!(Arc::ptr_eq(&snapshot[0],&root));assert!(root.belongs(&owner));}
        let mut b=boundary.lock().unwrap();assert!(b.closed);let closed=b.closed;
        assert!(b.slots.insert(closed,root).is_err());assert_eq!(b.close().len(),snapshot.len());
    }}
}

#[test]
fn finite_registry_keeps_failed_slots_and_refuses_foreign_or_duplicate_roots() {
    for limit in [1,COMMAND_ROOTS]{let mut slots=RootSlots::new(limit);let roots:Vec<_>=(0..=limit).map(|_|Arc::new(AtomicBool::new(false))).collect();
        for original in roots.iter().take(limit){slots.insert(false,original.clone()).unwrap();}
        assert!(slots.insert(false,roots[0].clone()).is_err());assert!(slots.insert(false,roots[limit].clone()).is_err());
        assert!(slots.require_member(&roots[limit]).is_err());slots.require_member(&roots[0]).unwrap();
        let retained=Arc::downgrade(&roots[0]);drop(roots);assert!(retained.upgrade().is_some());assert_eq!(slots.roots.len(),limit);
        assert!(slots.insert(false,Arc::new(AtomicBool::new(false))).is_err());
    }
}

#[test]
fn admitted_original_first_descendant_can_claim_after_close_but_attempt_never_recycles() {
    let owner=Arc::new(AtomicBool::new(false));let root=Arc::new(Root{actual:OriginalOwnerWeak::capture(&owner),children:OnceLock::<Arc<OriginalRole<AtomicBool>>>::new()});
    let mut boundary=Boundary{closed:false,slots:RootSlots::new(1)};boundary.slots.insert(false,root.clone()).unwrap();let snapshot=boundary.close();
    boundary.slots.require_member(&root).unwrap();assert!(Arc::ptr_eq(&root,&snapshot[0]));
    let actual=Arc::new(AtomicBool::new(false));let role=charge_role(&root.children,Arc::new(OriginalRole::capture(&actual))).unwrap();
    let failed=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{role.start_once(&actual).unwrap();panic!("actual post-claim test panic");}));assert!(failed.is_err());
    assert!(role.start_once(&actual).is_err());assert!(charge_role(&root.children,Arc::new(OriginalRole::capture(&actual))).is_err());
    assert!(Arc::ptr_eq(root.children.get().unwrap(),&role));assert!(boundary.slots.insert(boundary.closed,Arc::new(Root{actual:OriginalOwnerWeak::capture(&owner),children:OnceLock::new()})).is_err());
}

#[test]
fn exact_original_role_refuses_foreign_payload_without_spending_original_start() {
    let original=Arc::new(AtomicBool::new(false));let foreign=Arc::new(AtomicBool::new(false));let role=OriginalRole::capture(&original);
    assert!(role.start_once(&foreign).is_err());assert!(!role.started.load(Ordering::Acquire));
    role.start_once(&original).unwrap();assert!(role.start_once(&original).is_err());assert!(role.started.load(Ordering::Acquire));
}

struct TlsWait{entered:mpsc::SyncSender<()>,release:mpsc::Receiver<()>}
impl Drop for TlsWait{fn drop(&mut self){let _=self.entered.send(());let _=self.release.recv_timeout(Duration::from_secs(5));}}
thread_local!{static TLS:RefCell<Option<TlsWait>>=const{RefCell::new(None)};}
#[test]
fn weak_root_needs_external_whole_owner_and_actual_tls_join_never_seals_root() {
    let (at_tls,tls)=mpsc::sync_channel(1);let (release,wait)=mpsc::sync_channel(1);
    let actual=Arc::new(OwnedWorkerJoin::new(thread::spawn(move||TLS.with(|v|*v.borrow_mut()=Some(TlsWait{entered:at_tls,release:wait})))));
    let root=Arc::new(Root{actual:OriginalOwnerWeak::capture(&actual),children:OnceLock::<Arc<OriginalRole<AtomicBool>>>::new()});
    let mut boundary=Boundary{closed:false,slots:RootSlots::new(1)};boundary.slots.insert(false,root).unwrap();let snapshot=boundary.close();
    let external_whole_owner=vec![actual.clone()];drop(actual);tls.recv_timeout(Duration::from_secs(2)).unwrap();
    let original=snapshot[0].actual.upgrade().unwrap();original.start_join_observer().unwrap();assert!(original.try_joined().unwrap().is_none());
    release.send(()).unwrap();let joined=original.await_joined(Duration::from_secs(2)).unwrap();assert!(external_whole_owner[0].try_joined().unwrap().unwrap().same_worker(&joined));
    // A real joined original can still first expose a covered descendant. Join
    // is no command/canonical seal and not a complete producer observation.
    let child=Arc::new(AtomicBool::new(false));charge_role(&snapshot[0].children,Arc::new(OriginalRole::capture(&child))).unwrap();
    drop(original);drop(external_whole_owner);assert!(snapshot[0].actual.upgrade().is_none());assert!(boundary.closed);
}

#[test]
fn original_role_slots_stay_charged_when_payload_aliases_disappear() {
    let slots:[OnceLock<Arc<OriginalRole<AtomicBool>>>;2]=[OnceLock::new(),OnceLock::new()];
    let first=Arc::new(AtomicBool::new(false));let second=Arc::new(AtomicBool::new(false));
    charge_role(&slots[0],Arc::new(OriginalRole::capture(&first))).unwrap();charge_role(&slots[1],Arc::new(OriginalRole::capture(&second))).unwrap();
    drop(first);drop(second);assert!(slots.iter().all(|s|s.get().unwrap().actual.upgrade().is_none()));
    for slot in &slots{assert!(charge_role(slot,Arc::new(OriginalRole::capture(&Arc::new(AtomicBool::new(false))))).is_err());}
}

#[test]
fn typed_original_weak_preserves_allocation_and_refuses_foreign_owner() {
    let actual=Arc::new(AtomicU8::new(7));let foreign=Arc::new(AtomicU8::new(7));
    let original=OriginalOwnerWeak::capture(&actual);
    let upgraded=original.upgrade().unwrap();assert!(Arc::ptr_eq(&actual,&upgraded));
    assert!(original.matches(&actual));assert!(!original.matches(&foreign));
    upgraded.store(9,Ordering::Release);assert_eq!(actual.load(Ordering::Acquire),9);
    assert_eq!(foreign.load(Ordering::Acquire),7);
}

#[test]
fn typed_original_weak_refuses_wrong_erased_payload_type() {
    // Exercise the actual downcast refusal using a deliberately malformed
    // PRIVATE carrier. No production API can construct this wrong-type link.
    let actual:Arc<dyn Any+Send+Sync>=Arc::new(AtomicBool::new(false));
    let wrong=OriginalOwnerWeak::<AtomicU8>{actual:Arc::downgrade(&actual),kind:PhantomData};
    assert!(wrong.upgrade().is_none());
    assert!(!wrong.matches(&Arc::new(AtomicU8::new(0))));
}

#[test]
fn typed_original_weak_does_not_retain_dropped_whole_owner() {
    let actual=Arc::new(AtomicU8::new(1));let original=OriginalOwnerWeak::capture(&actual);
    let external_whole_owner=actual.clone();drop(actual);
    assert!(Arc::ptr_eq(&original.upgrade().unwrap(),&external_whole_owner));
    drop(external_whole_owner);assert!(original.upgrade().is_none());
}

#[test]
fn actual_covered_original_payloads_and_roots_are_send_sync() {
    fn checked<T:Any+Send+Sync>(){}
    // Real payload obligations, not only erased trait-object assertions. These
    // remain compiler checked at every production capture/upgrade call too.
    checked::<IngressJob>();checked::<FreshInitializationAttempt>();
    checked::<SharedRuntimeEnvironment>();checked::<FlowCell>();
    checked::<Cell<ProjectCarrier>>();checked::<Cell<seeds::SeedCarrier>>();
    checked::<IngressCommandRoot>();checked::<FreshInitializationRoot>();
    checked::<OriginalTaskCommand>();checked::<OriginalFreshEnvironment>();checked::<OriginalFreshFlow>();
}
