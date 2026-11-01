//! Actual original JoinHandle/TLS and linear-permit regressions. Seed refs are
//! DATA for the private process-local ledger; no Root/mount/ACK is fabricated.
use super::*;
use rudder_engine::{execution::handoff::OwnedWorkerJoin,protocol::{ObjectRef,ObjectKind}};
use std::{cell::RefCell,collections::BTreeSet,sync::mpsc::{self,SyncSender,Receiver}};
fn original()->FinalizationBudget {FinalizationBudget::begin(Duration::from_secs(20)).unwrap()}
fn scope(ledger:&SeedUseLedger)->(Option<SeedUseGuard>,Option<SeedUseGuard>,SeedTaskUseGate) {
    let task=Some(ledger.acquire(SeedUseKind::Task).unwrap());let fork=Some(ledger.acquire(SeedUseKind::Fork).unwrap());let mut gate=None;
    ledger.register_task_start_retaining(&mut gate,task.as_ref().unwrap(),fork.as_ref().unwrap(),
        &ObjectRef::from_bytes(ObjectKind::Directory,b"original fixture seed DATA"),&original()).unwrap();(task,fork,gate.unwrap())
}
fn bind_returned(gate:&SeedTaskUseGate)->OwnedWorkerJoin {
    let worker=OwnedWorkerJoin::new(thread::spawn(||{}));gate.bind_original_factory(&worker).unwrap();worker.await_joined(Duration::from_secs(5)).unwrap();worker
}
fn close_no_effect(value:&mut Option<&'static str>)->Result<()> {value.take();Ok(())}
fn finish(guard:&mut Option<SeedUseGuard>,budget:&FinalizationBudget) {
    let mut value=Some(Some("actual retained unused owner"));let mut work=None;
    RetainedSeedJoin::stage_retaining(&mut work,guard,&mut value,budget).unwrap();
    let mut work=work.unwrap();work.start_retaining(close_no_effect).unwrap();work.await_joined().unwrap();
}
#[test]
fn original_unspent_permits_and_actual_factory_join_allow_zero_helper_use_retirement_only_after_both_cleanup_joins() {
    let ledger=SeedUseLedger::new();let (mut task,mut fork,gate)=scope(&ledger);let _original_worker=bind_returned(&gate);
    let budget=original();let cancel=AtomicBool::new(false);let mut seal=None;gate.seal_retaining(&mut seal,&budget,&cancel).unwrap();
    let seal=seal.unwrap();assert!(seal.before_dispatch()&&seal.fork_was_never_claimed());
    assert!(gate.claim_dispatch().is_err());assert!(gate.claim_helper_start().is_err());assert!(gate.claim_enrollment().is_err());
    let mut ack=None;ledger.acknowledge_never_started_retaining(&mut ack,task.as_ref().unwrap(),fork.as_ref().unwrap(),&seal,&mut None,&mut None,&budget,&cancel).unwrap();
    // Per-task seal/ACK does not close sibling admission.
    let mut sibling=Some(ledger.acquire(SeedUseKind::Cache).unwrap());
    ledger.close().unwrap();finish(&mut fork,&budget);finish(&mut task,&budget);
    assert!(ledger.prove(&BTreeSet::new(),&budget,&cancel).is_err(),"unknown sibling stays charged");finish(&mut sibling,&budget);
    assert!(ledger.prove(&BTreeSet::new(),&budget,&cancel).is_ok());
    drop(ack);assert!(ledger.prove(&BTreeSet::new(),&budget,&cancel).is_ok(),"ledger retains actual original ACK after lost output");
}
struct Tls {entered:SyncSender<()>,release:Receiver<()>}
impl Drop for Tls {fn drop(&mut self){self.entered.send(()).unwrap();self.release.recv_timeout(Duration::from_secs(5)).unwrap();}}
thread_local! {static LIVE_TLS:RefCell<Option<Tls>>=const {RefCell::new(None)};}
#[test]
fn returned_factory_callback_cannot_seal_before_actual_original_tls_destructor_join() {
    let ledger=SeedUseLedger::new();let (_,_,gate)=scope(&ledger);let (entered,saw)=mpsc::sync_channel(1);let (release,wait)=mpsc::sync_channel(1);
    let worker=OwnedWorkerJoin::new(thread::spawn(move||LIVE_TLS.with(|tls|*tls.borrow_mut()=Some(Tls {entered,release:wait}))));
    gate.bind_original_factory(&worker).unwrap();saw.recv_timeout(Duration::from_secs(5)).unwrap();worker.start_join_observer().unwrap();
    let budget=original();let mut seal=None;assert!(gate.seal_retaining(&mut seal,&budget,&AtomicBool::new(false)).is_err());assert!(seal.is_none());
    release.send(()).unwrap();worker.await_joined(Duration::from_secs(5)).unwrap();gate.seal_retaining(&mut seal,&budget,&AtomicBool::new(false)).unwrap();assert!(seal.is_some());
}
#[test]
fn helper_or_enrollment_attempt_permanently_refuses_never_started_even_after_successful_factory_join() {
    for enroll in [false,true] {
        let ledger=SeedUseLedger::new();let (_,_,gate)=scope(&ledger);let _worker=bind_returned(&gate);
        gate.claim_dispatch().unwrap();gate.claim_helper_start().unwrap();if enroll {gate.claim_enrollment().unwrap();}
        let mut seal=None;assert!(gate.seal_retaining(&mut seal,&original(),&AtomicBool::new(false)).is_err());assert!(seal.is_none());
        ledger.close().unwrap();assert!(ledger.prove(&BTreeSet::new(),&original(),&AtomicBool::new(false)).is_err());
    }
}
#[test]
fn claimed_fork_creation_needs_genuine_vcs_dependency_receipt_and_foreign_seals_never_close_a_use() {
    let ledger=SeedUseLedger::new();let (task,fork,gate)=scope(&ledger);let _worker=bind_returned(&gate);gate.claim_dispatch().unwrap();gate.claim_fork_creation().unwrap();
    let budget=original();let mut seal=None;gate.seal_retaining(&mut seal,&budget,&AtomicBool::new(false)).unwrap();let seal=seal.unwrap();
    assert!(!seal.before_dispatch()&&!seal.fork_was_never_claimed());
    assert!(ledger.acknowledge_never_started_retaining(&mut None,task.as_ref().unwrap(),fork.as_ref().unwrap(),&seal,&mut None,&mut None,&budget,&AtomicBool::new(false)).is_err());
    let other=SeedUseLedger::new();let (foreign_task,foreign_fork,foreign_gate)=scope(&other);let _worker=bind_returned(&foreign_gate);
    let mut foreign=None;foreign_gate.seal_retaining(&mut foreign,&budget,&AtomicBool::new(false)).unwrap();assert!(gate.matches_seal(foreign.as_ref().unwrap())==false);
    assert!(other.acknowledge_never_started_retaining(&mut None,foreign_task.as_ref().unwrap(),foreign_fork.as_ref().unwrap(),&seal,&mut None,&mut None,&budget,&AtomicBool::new(false)).is_err());
}
#[test]
fn original_factory_panic_or_expired_cleanup_keeps_real_owner_and_cannot_be_renewed() {
    let ledger=SeedUseLedger::new();let (_,_,gate)=scope(&ledger);let worker=OwnedWorkerJoin::new(thread::spawn(||panic!("original factory failed")));
    gate.bind_original_factory(&worker).unwrap();assert!(worker.await_joined(Duration::from_secs(5)).is_err());let mut seal=None;
    assert!(gate.seal_retaining(&mut seal,&original(),&AtomicBool::new(false)).is_err());assert!(seal.is_none());
    let ledger=SeedUseLedger::new();let (_,_,gate)=scope(&ledger);let _worker=bind_returned(&gate);let budget=original();
    gate.seal_retaining(&mut seal,&budget,&AtomicBool::new(false)).unwrap();budget.cancel();
    assert!(gate.seal_retaining(&mut seal,&original(),&AtomicBool::new(false)).is_err(),"fresh clock cannot replace original cleanup");
    assert!(gate.claim_dispatch().is_err());
}
#[test]
fn helper_start_racing_the_seal_can_only_win_one_original_permit() {
    let ledger=SeedUseLedger::new();let (_,_,gate)=scope(&ledger);let _worker=bind_returned(&gate);gate.claim_dispatch().unwrap();
    let contender=gate.clone();let worker=thread::spawn(move||contender.claim_helper_start().is_ok());
    let mut seal=None;let budget=original();let first=gate.seal_retaining(&mut seal,&budget,&AtomicBool::new(false));let helper=worker.join().unwrap();
    if helper {assert!(seal.is_none());assert!(gate.seal_retaining(&mut seal,&budget,&AtomicBool::new(false)).is_err());}
    else {if first.is_err(){gate.seal_retaining(&mut seal,&budget,&AtomicBool::new(false)).unwrap();}assert!(seal.is_some());assert!(gate.claim_helper_start().is_err());}
}
#[test]
fn admission_owner_stays_charged_until_actual_cleanup_tls_join_then_explicit_joined_finalization() {
    struct Admission(Arc<AtomicUsize>);impl Drop for Admission {fn drop(&mut self){self.0.fetch_sub(1,Ordering::AcqRel);}}
    struct Held {admission:Option<Admission>,tls:Option<(SyncSender<()>,Receiver<()>)>}
    fn callback(held:&mut Held)->Result<()> {let (entered,release)=held.tls.take().unwrap();LIVE_TLS.with(|tls|*tls.borrow_mut()=Some(Tls {entered,release}));Ok(())}
    let ledger=SeedUseLedger::new();let mut guard=Some(ledger.acquire(SeedUseKind::Cache).unwrap());let budget=original();
    let charged=Arc::new(AtomicUsize::new(1));let (entered,saw)=mpsc::sync_channel(1);let (release,wait)=mpsc::sync_channel(1);
    let mut value=Some(Held {admission:Some(Admission(charged.clone())),tls:Some((entered,wait))});let mut work=None;
    RetainedSeedJoin::stage_retaining(&mut work,&mut guard,&mut value,&budget).unwrap();let mut work=work.unwrap();work.start_retaining(callback).unwrap();
    saw.recv_timeout(Duration::from_secs(5)).unwrap();work.start_observer().unwrap();assert!(work.reconcile_joined().unwrap().is_none());assert_eq!(charged.load(Ordering::Acquire),1);
    release.send(()).unwrap();let joined=work.await_joined().unwrap();assert_eq!(charged.load(Ordering::Acquire),1,"real join receipt alone does not drop the retained admission");
    joined.with_value_mut(|held|{drop(held.admission.take());Ok(())}).unwrap();assert_eq!(charged.load(Ordering::Acquire),0);
}
