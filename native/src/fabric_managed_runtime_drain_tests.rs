//! Local actual threads/TLS only. No helper/backend process-exit claim.
use super::*;
use crate::managed_launch::{ManagedLaunchFence, WorkspaceAssignment};
use rudder_engine::{execution::ProtectedHostContextRef, protocol::{ObjectKind, ObjectRef},
    vfs::rpc::{AttemptContext, ObservationBinding}};
use std::{cell::RefCell, sync::{atomic::AtomicUsize, mpsc}};

fn assignment() -> WorkspaceAssignment {
    let fence = ManagedLaunchFence { owner: OwnerFence { project_id: "project".into(), owner_id: "owner".into(), epoch: 7 },
        run_id: "run".into(), run_epoch: 3, attempt_id: uuid::Uuid::new_v4().to_string(), request_id: uuid::Uuid::new_v4().to_string() };
    let reference = |kind, bytes: &[u8]| ObjectRef::from_bytes(kind, bytes);
    WorkspaceAssignment { format_version: 1, runnable_root: reference(ObjectKind::RunnableRoot,b"root"),
        source: reference(ObjectKind::SourceRoot,b"source"), seed: reference(ObjectKind::Directory,b"seed"),
        observation: ObservationBinding { format_version: 1, attempt: AttemptContext { execution_id: fence.attempt_id.clone(), operation_id: uuid::Uuid::new_v4().to_string() },
            view_id: uuid::Uuid::new_v4().to_string(), owner: fence.owner_epoch(), namespace: uuid::Uuid::new_v4().to_string(),
            profile: reference(ObjectKind::Blob,b"profile"), exclusion_policy_digest: "a".repeat(64) },
        host_context: ProtectedHostContextRef { format_version: 1, path: "/missing/runtime-drain-context.json".into(), sha256: "b".repeat(64), size_bytes: 1 }, fence }
}
fn slot(assignment: &WorkspaceAssignment) -> RuntimeLink {
    RuntimeLink { attempt: assignment.fence.attempt_id.clone(), epoch: assignment.fence.run_epoch,
        profile: "claude-sdk".into(), backend: Backend::Claude, transport_uncertain: false,
        link: None, broker: None, broker_work: None, projection_hash: None, broker_failed_hash: None,
        thread: None, active: None, interrupted: None, phase: RuntimePhase::Starting, qualified: false,
        uncertain: false, closed: false, draining: false, workspace: Some(assignment.clone()),
        managed_home: Some("/missing/runtime-drain-home".into()), owned_backend: None }
}
struct TlsGate { entered: mpsc::SyncSender<()>, release: Receiver<()>, finished: Arc<AtomicUsize> }
impl Drop for TlsGate {
    fn drop(&mut self) { let _ = self.entered.send(()); let _ = self.release.recv_timeout(Duration::from_secs(5)); self.finished.fetch_add(1, Ordering::AcqRel); }
}
thread_local! { static EXIT_GATE: RefCell<Option<TlsGate>> = const { RefCell::new(None) }; }
fn held_work(assignment: &WorkspaceAssignment, panic_after_result: bool) ->
    (ManagedBrokerWork, Receiver<()>, mpsc::SyncSender<()>, Arc<AtomicUsize>) {
    let (result_tx, result_rx) = mpsc::sync_channel(1); let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1); let finished = Arc::new(AtomicUsize::new(0));
    let dropping = finished.clone(); let identity = assignment.clone();
    let thread = std::thread::spawn(move || {
        EXIT_GATE.with(|gate| *gate.borrow_mut() = Some(TlsGate { entered: entered_tx, release: release_rx, finished: dropping }));
        result_tx.send(ManagedBrokerResult { run_id: identity.fence.run_id, attempt: identity.fence.attempt_id,
            run_epoch: identity.fence.run_epoch, owner_epoch: identity.fence.owner.epoch, hash: [7;32], result: Ok(None) }).unwrap();
        if panic_after_result { panic!("fixture namespace worker panic after result"); }
    });
    let worker = OwnedWorkerJoin::new(thread); worker.start_join_observer().unwrap(); // Off-App test owner.
    (ManagedBrokerWork { assignment: assignment.clone(), hash: [7;32], cancelled: Arc::new(AtomicBool::new(false)),
        worker, receiver: result_rx, result: None, joined: None, completed: false, projection: None }, entered_rx, release_tx, finished)
}
fn install(state: &mut RuntimeState, work: ManagedBrokerWork) {
    let id = work.assignment.fence.run_id.clone(); let mut runtime = slot(&work.assignment);
    runtime.broker_work = Some(BrokerWork { hash: work.hash, cancelled: work.cancelled.clone() });
    state.links.insert(id.clone(), runtime); state.managed_work.insert(id, work);
}

#[test]
fn pure_exact_take_does_not_reopen_missing_host_paths_or_claim_physical_exit() {
    let assignment = assignment(); let mut state = RuntimeState::default();
    state.links.insert("run".into(),slot(&assignment));
    let mut drain = state.take_managed_drain(&assignment).unwrap();
    assert!(state.links.is_empty()); assert_eq!(drain.assignment(),&assignment);
    let joined = drain.await_closed(Duration::from_secs(1)).unwrap(); assert_eq!(joined.assignment(),&assignment);
    assert!(joined._link.is_none() && joined._work.is_none(), "no actor/work was started; this says nothing about helper/backend");
    assert!(drain.await_closed(Duration::from_secs(1)).is_err());
}

#[test]
fn full_assignment_mismatch_preserves_actual_thread_owner_and_slot() {
    let assignment = assignment(); let (work, entered, release, finished) = held_work(&assignment,false);
    entered.recv_timeout(Duration::from_secs(1)).unwrap(); let mut state = RuntimeState::default(); install(&mut state,work);
    let mut foreign = assignment.clone(); foreign.source = ObjectRef::from_bytes(ObjectKind::SourceRoot,b"foreign-source");
    assert!(state.take_managed_drain(&foreign).is_err()); assert_eq!(state.links.len(),1); assert_eq!(state.managed_work.len(),1);
    assert_eq!(finished.load(Ordering::Acquire),0);
    let mut drain = state.take_managed_drain(&assignment).unwrap(); release.send(()).unwrap();
    let _joined = drain.await_closed(Duration::from_secs(1)).unwrap(); assert_eq!(finished.load(Ordering::Acquire),1);
}

#[test]
fn received_result_and_close_cannot_clear_unjoined_tls_worker() {
    let assignment = assignment(); let (mut work, entered, release, finished) = held_work(&assignment,false);
    entered.recv_timeout(Duration::from_secs(1)).unwrap(); work.receive(); assert!(work.result.is_some());
    assert!(!work.observe_joined().unwrap()); assert!(work.finish().is_err()); assert!(!work.completed);
    let mut state = RuntimeState::default(); install(&mut state,work); state.close_managed_assignment(&assignment);
    let mut drain = state.take_managed_drain(&assignment).unwrap(); assert!(drain.work.as_ref().unwrap().result.is_some());
    assert!(drain.await_closed(Duration::from_millis(20)).is_err()); assert_eq!(finished.load(Ordering::Acquire),0);
    assert!(drain.work.is_some() && !drain.settled);
    release.send(()).unwrap(); let joined = drain.await_closed(Duration::from_secs(1)).unwrap();
    assert!(joined._work.is_some()); assert_eq!(finished.load(Ordering::Acquire),1);
}

#[test]
fn already_consumed_result_keeps_same_actual_join_owner_until_extraction() {
    let assignment = assignment(); let (mut work, entered, release, _) = held_work(&assignment,false);
    entered.recv_timeout(Duration::from_secs(1)).unwrap(); release.send(()).unwrap();
    let original = work.worker.await_joined(Duration::from_secs(1)).unwrap();
    assert!(work.observe_joined().unwrap()); work.finish().unwrap(); assert!(work.completed);
    let mut state = RuntimeState::default(); install(&mut state,work);
    assert_eq!(state.managed_work.len(),1,"one retained completed owner, not a per-run history list");
    let mut drain = state.take_managed_drain(&assignment).unwrap(); let joined = drain.await_closed(Duration::from_secs(1)).unwrap();
    assert!(joined._work.as_ref().unwrap().same_worker(&original));
}

#[test]
fn unwinding_waiter_keeps_actual_carrier_for_explicit_retry() {
    let assignment = assignment(); let (work, entered, release, finished) = held_work(&assignment,false);
    entered.recv_timeout(Duration::from_secs(1)).unwrap(); let mut state = RuntimeState::default(); install(&mut state,work);
    let mut carrier = state.take_managed_drain(&assignment).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(carrier.await_closed(Duration::from_millis(10)).is_err()); panic!("fixture caller panic");
    }));
    assert!(result.is_err()); assert!(carrier.work.is_some()); assert_eq!(finished.load(Ordering::Acquire),0);
    release.send(()).unwrap(); let _joined = carrier.await_closed(Duration::from_secs(1)).unwrap();
    assert_eq!(finished.load(Ordering::Acquire),1);
}

#[test]
fn panicked_namespace_worker_never_promotes_its_received_result_to_joined_success() {
    let assignment = assignment(); let (work, entered, release, finished) = held_work(&assignment,true);
    entered.recv_timeout(Duration::from_secs(1)).unwrap(); let mut state = RuntimeState::default(); install(&mut state,work);
    let mut drain = state.take_managed_drain(&assignment).unwrap(); release.send(()).unwrap();
    assert!(drain.await_closed(Duration::from_secs(1)).is_err()); assert_eq!(finished.load(Ordering::Acquire),1);
    assert!(drain.await_closed(Duration::from_millis(10)).is_err()); assert!(!drain.settled && drain.work.is_some());
}

#[test]
fn stale_private_result_stays_owned_and_cannot_settle_another_assignment() {
    let assignment = assignment(); let (mut work, entered, release, _) = held_work(&assignment,false);
    entered.recv_timeout(Duration::from_secs(1)).unwrap(); work.receive(); work.result.as_mut().unwrap().owner_epoch += 1;
    let mut state = RuntimeState::default(); install(&mut state,work); let mut drain = state.take_managed_drain(&assignment).unwrap();
    release.send(()).unwrap(); assert!(drain.await_closed(Duration::from_secs(1)).is_err());
    assert!(drain.work.as_ref().unwrap().result.is_some() && !drain.settled);
}

#[test]
fn untracked_managed_work_refuses_extraction_instead_of_inventing_a_join_witness() {
    let assignment = assignment(); let mut state = RuntimeState::default(); let mut runtime = slot(&assignment);
    runtime.broker_work = Some(BrokerWork { hash:[1;32], cancelled:Arc::new(AtomicBool::new(false)) });
    state.links.insert("run".into(),runtime); assert!(state.take_managed_drain(&assignment).is_err()); assert_eq!(state.links.len(),1);
}

#[test]
fn actual_drain_carriers_and_join_tokens_can_cross_the_off_app_worker_boundary() {
    fn send<T: Send + 'static>() {}
    send::<ManagedRuntimeDrain>(); send::<ManagedRuntimeJoined>();
}
