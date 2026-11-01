//! Actual local thread/TLS custody tests. UUID associations exercise ONLY the
//! private native ledger; they never issue Root/helper/publication authority.
use super::*;
use std::{cell::RefCell,collections::BTreeSet,sync::mpsc::{self,Receiver,SyncSender},time::Instant};
fn budget()->FinalizationBudget {FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
struct Probe {drops:Arc<AtomicUsize>,calls:usize,cancel:Option<FinalizationBudget>,
    tls:Option<(SyncSender<()>,Receiver<()>)>,returned:Option<SyncSender<()>>}
impl Probe {fn new(drops:Arc<AtomicUsize>)->Self {Self {drops,calls:0,cancel:None,tls:None,returned:None}}}
impl Drop for Probe {fn drop(&mut self){self.drops.fetch_add(1,Ordering::AcqRel);}}
struct TlsBlock {entered:SyncSender<()>,release:Receiver<()>}
impl Drop for TlsBlock {fn drop(&mut self){self.entered.send(()).unwrap();self.release.recv_timeout(Duration::from_secs(10)).unwrap();}}
thread_local! {static HELD_TLS:RefCell<Option<TlsBlock>>=const {RefCell::new(None)};}
fn work(probe:&mut Probe)->Result<()> {
    probe.calls+=1;
    if let Some((entered,release))=probe.tls.take(){HELD_TLS.with(|cell|*cell.borrow_mut()=Some(TlsBlock {entered,release}));}
    if let Some(cancel)=&probe.cancel {cancel.cancel();}
    if let Some(returned)=probe.returned.take(){returned.send(()).unwrap();}Ok(())
}
fn panic_work(probe:&mut Probe)->Result<()> {probe.calls+=1;panic!("actual original worker panic, value retained");}
fn stage(ledger:&SeedUseLedger,probe:Probe,original:&FinalizationBudget)->RetainedSeedJoin<Probe> {
    let mut guard=Some(ledger.acquire(SeedUseKind::Cache).unwrap());let mut value=Some(probe);let mut owner=None;
    RetainedSeedJoin::stage_retaining(&mut owner,&mut guard,&mut value,original).unwrap();
    assert!(guard.is_none()&&value.is_none());owner.unwrap()
}
fn spin(mut condition:impl FnMut()->bool){let end=Instant::now()+Duration::from_secs(5);while !condition(){assert!(Instant::now()<end);thread::sleep(Duration::from_millis(1));}}

#[test]
fn actual_tls_join_holds_bounded_workers_and_all_original_values_after_dropped_drivers() {
    let ledger=SeedUseLedger::new();let original=budget();let drops=Arc::new(AtomicUsize::new(0));
    let mut owners=Vec::new();let mut releases=Vec::new();let mut entered=Vec::new();
    for _ in 0..3 {
        let (tx,rx)=mpsc::sync_channel(1);let (release,wait)=mpsc::sync_channel(1);
        let mut probe=Probe::new(drops.clone());probe.tls=Some((tx,wait));
        owners.push(stage(&ledger,probe,&original));releases.push(release);entered.push(rx);
    }
    owners[0].start_retaining(work).unwrap();owners[1].start_retaining(work).unwrap();
    assert!(owners[2].start_retaining(work).is_err());
    entered[0].recv_timeout(Duration::from_secs(5)).unwrap();entered[1].recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(ledger.workers.load(Ordering::Acquire),2);
    ledger.close().unwrap();assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_err());
    // Neither callback return nor losing the typed driver can free a worker.
    let mut third=owners.pop().unwrap();drop(owners);ledger.drive_joins_off_app().unwrap();
    assert!(third.start_retaining(work).is_err());assert_eq!(drops.load(Ordering::Acquire),0);
    releases[0].send(()).unwrap();releases[1].send(()).unwrap();
    spin(||ledger.workers.load(Ordering::Acquire)==0);
    third.start_retaining(work).unwrap();entered[2].recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_err());releases[2].send(()).unwrap();
    let joined=third.await_joined().unwrap();drop(joined);drop(third);
    let proof=ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).unwrap();assert_eq!(drops.load(Ordering::Acquire),0);
    drop(proof);drop(ledger);spin(||drops.load(Ordering::Acquire)==3);
}

#[test]
fn known_joined_sibling_cannot_release_unknown_or_dropped_seed_use() {
    let ledger=SeedUseLedger::new();let drops=Arc::new(AtomicUsize::new(0));
    let mut known=stage(&ledger,Probe::new(drops.clone()),&budget());known.start_retaining(work).unwrap();
    drop(ledger.acquire(SeedUseKind::Cache).unwrap()); // No actual owner/join returned.
    let receipt=known.await_joined().unwrap();ledger.close().unwrap();
    assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_err());drop(receipt);drop(known);
    assert_eq!(drops.load(Ordering::Acquire),0);assert!(ledger.acquire(SeedUseKind::Fork).is_err());
    assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_err());
}

#[test]
fn callback_return_without_actual_join_is_never_native_actor_drain() {
    let ledger=SeedUseLedger::new();let (returned,rx)=mpsc::sync_channel(1);
    let mut probe=Probe::new(Arc::new(AtomicUsize::new(0)));probe.returned=Some(returned);
    let mut owner=stage(&ledger,probe,&budget());owner.start_retaining(work).unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();ledger.close().unwrap();
    assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_err());owner.await_joined().unwrap();
    assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_ok());
}

#[test]
fn original_panic_and_value_survive_actual_join_and_explicit_same_owner_reconciliation() {
    let ledger=SeedUseLedger::new();let drops=Arc::new(AtomicUsize::new(0));
    let mut owner=stage(&ledger,Probe::new(drops.clone()),&budget());owner.start_retaining(panic_work).unwrap();
    assert!(owner.await_joined().is_err());ledger.close().unwrap();assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_err());
    assert_eq!(drops.load(Ordering::Acquire),0);
    owner.retry_failed_retaining(work).unwrap();let joined=owner.await_joined().unwrap();
    assert_eq!(joined.with_value(|probe|Ok(probe.calls)).unwrap(),2);
    assert!(matches!(&owner.state.join.lock().unwrap().prior[0].1,Err(_)));
    assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_ok());assert_eq!(drops.load(Ordering::Acquire),0);
}

#[test]
fn late_callback_cannot_renew_original_budget_or_emit_join_success() {
    let ledger=SeedUseLedger::new();let original=budget();let drops=Arc::new(AtomicUsize::new(0));
    let mut probe=Probe::new(drops.clone());probe.cancel=Some(original.clone());
    let mut owner=stage(&ledger,probe,&original);owner.start_retaining(work).unwrap();
    owner.start_observer().unwrap();spin(||owner.state.join.lock().unwrap().result.is_some());
    ledger.close().unwrap();assert!(owner.reconcile_joined().is_err());assert!(owner.retry_failed_retaining(work).is_err());
    assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_err());assert_eq!(drops.load(Ordering::Acquire),0);
}

#[test]
fn pre_effect_stage_refusal_preserves_exact_original_guard_and_resource() {
    let ledger=SeedUseLedger::new();let original=budget();original.cancel();
    let mut guard=Some(ledger.acquire(SeedUseKind::Template).unwrap());let drops=Arc::new(AtomicUsize::new(0));
    let mut value=Some(Probe::new(drops.clone()));let mut out=None;
    assert!(RetainedSeedJoin::stage_retaining(&mut out,&mut guard,&mut value,&original).is_err());
    assert!(out.is_none()&&guard.is_some()&&value.is_some());assert_eq!(drops.load(Ordering::Acquire),0);
}

#[test]
fn helper_association_is_exact_owner_one_task_and_full_reader_set() {
    let ledger=SeedUseLedger::new();let other=SeedUseLedger::new();let first=ledger.acquire(SeedUseKind::Task).unwrap();
    let second=ledger.acquire(SeedUseKind::Task).unwrap();let foreign=other.acquire(SeedUseKind::Task).unwrap();
    let id=uuid::Uuid::new_v4();assert!(ledger.associate(&foreign,id).is_err());
    ledger.associate(&first,id).unwrap();ledger.associate(&first,id).unwrap();
    assert!(ledger.associate(&second,id).is_err());assert!(ledger.associate(&first,uuid::Uuid::new_v4()).is_err());
    ledger.close().unwrap();assert!(ledger.prove(&BTreeSet::from([id]),&budget(),&AtomicBool::new(false)).is_err());
}

#[test]
fn dropped_uses_never_reclaim_lifetime_slots_or_enable_a_false_empty_proof() {
    let ledger=SeedUseLedger::new();assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_err());
    for _ in 0..MAX_USES {drop(ledger.acquire(SeedUseKind::Fork).unwrap());}
    assert!(ledger.acquire(SeedUseKind::Fork).is_err());ledger.close().unwrap();
    assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_err());
    // Fresh generation capacity is independent; old unresolved generation is
    // still held. This is ledger isolation, never a Root capacity-release ACK.
    assert!(SeedUseLedger::new().acquire(SeedUseKind::Template).is_ok());
}

#[test]
fn actual_joined_task_or_fork_cannot_discard_persistent_shared_object_alternates() {
    for kind in [SeedUseKind::Task,SeedUseKind::Fork] {
        let ledger=SeedUseLedger::new();let mut guard=Some(ledger.acquire(kind).unwrap());
        let mut readers=BTreeSet::new();
        if kind==SeedUseKind::Task {let id=uuid::Uuid::new_v4();ledger.associate(guard.as_ref().unwrap(),id).unwrap();readers.insert(id);}
        let mut value=Some(Probe::new(Arc::new(AtomicUsize::new(0))));let mut out=None;
        RetainedSeedJoin::stage_retaining(&mut out,&mut guard,&mut value,&budget()).unwrap();
        let owner=out.as_mut().unwrap();owner.start_retaining(work).unwrap();owner.await_joined().unwrap();
        ledger.close().unwrap();let error=ledger.prove(&readers,&budget(),&AtomicBool::new(false)).err().unwrap();
        assert!(error.to_string().contains("durable fork localization ACK absent"));
    }
}

#[test]
fn original_attempt_data_binding_is_one_shot_and_never_a_localization_or_join_receipt() {
    use rudder_engine::protocol::{ObjectKind,ObjectRef,OwnerEpoch};
    use rudder_vcs_bridge::{CommitIdentity,GitCommitId,JjChangeId,JjTreeIdentity,GitTreeId,JjOperationId};
    let binding=AttemptBinding {format_version:1,project_id:"p".into(),owner:OwnerEpoch {owner:"o".into(),epoch:1},run_id:"r".into(),run_epoch:1,
        attempt_id:"a".into(),view_id:"v".into(),seed:ObjectRef::from_bytes(ObjectKind::Directory,b"seed"),
        baseline:CommitIdentity {commit:GitCommitId("a".repeat(40)),change:JjChangeId("b".repeat(32)),parents:vec![GitCommitId("c".repeat(40))],
            tree:JjTreeIdentity {terms:vec![GitTreeId("d".repeat(40))],labels:Vec::new()}},baseline_operation:JjOperationId("e".repeat(128)),
        projection_policy:ObjectRef::from_bytes(ObjectKind::Blob,b"policy"),profile:ObjectRef::from_bytes(ObjectKind::Blob,b"profile")};
    let ledger=SeedUseLedger::new();let task=ledger.acquire(SeedUseKind::Task).unwrap();task.bind_original_attempt(&binding).unwrap();
    task.bind_original_attempt(&binding).unwrap();let mut changed=binding.clone();changed.attempt_id="replacement".into();
    assert!(task.bind_original_attempt(&changed).is_err());ledger.close().unwrap();
    assert!(ledger.prove(&BTreeSet::new(),&budget(),&AtomicBool::new(false)).is_err(),"DATA plus closure is never actual joins/localization");
}
