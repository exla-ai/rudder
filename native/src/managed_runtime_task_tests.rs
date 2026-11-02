//! Actual original child/pidfd/staged barrier mechanism only. This creates no
//! Root runtime client, runtime ACK, seed ACK, image authority or backend run.
use super::*;
use rudder_engine::execution::handoff::{ManagedExecutionHandoff, RetainedManagedExecutionActivation};
use std::{process::{Child, Command, Stdio}, sync::mpsc};

struct Fixture {
    _source: crate::source_issuer::SourceIssuerFixture,
    _prepared: crate::managed_launch::source_prepare::PreparedManagedSource,
    activation: RetainedManagedExecutionActivation,
    start: Option<Arc<RuntimeTaskStart>>,
    child: Option<Child>,
    original: FinalizationBudget,
    cancelled: Arc<AtomicBool>,
}
impl Fixture {
    fn new() -> Self {
        let (mut source, request) = crate::managed_launch::source_prepare::tests::request();
        let prepared = crate::managed_launch::source_prepare::prepare_owned(request, source.journal(), &AtomicBool::new(false)).unwrap();
        let handoff = ManagedExecutionHandoff::new(prepared.execution_spec(), prepared.binding().clone()).unwrap();
        let activation = RetainedManagedExecutionActivation::new(handoff);
        let child = Command::new("/bin/sleep").arg("30").env_clear().stdin(Stdio::null())
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let mut fixture = Self { _source: source, _prepared: prepared, activation,
            start: None, child: Some(child), original: FinalizationBudget::begin(Duration::from_secs(30)).unwrap(),
            cancelled: Arc::new(AtomicBool::new(false)) };
        fixture.activation.stage_bounded(fixture.child.as_ref().unwrap().id(), &fixture.original, fixture.cancelled.clone()).unwrap();
        fixture.start = Some(RuntimeTaskStart::capture(fixture.activation.take_start_barrier().unwrap()));
        fixture
    }
    fn start(&self) -> Arc<RuntimeTaskStart> { Arc::clone(self.start.as_ref().unwrap()) }
    fn finish(mut self) {
        // Actual undispatched worker settlement uses its separate explicit
        // cleanup request; it never extends the original task/grant clock.
        self.start.take();
        let cleanup = FinalizationBudget::begin(Duration::from_secs(5)).unwrap();
        self.activation.settle_undispatched_off_app(&cleanup).unwrap();
        assert!(self.activation.is_settled());
        let child = self.child.as_mut().unwrap();
        assert!(child.try_wait().unwrap().is_none(), "worker join did not imply helper exit");
        child.kill().unwrap(); assert!(!child.wait().unwrap().success()); self.child.take();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Test failure containment only; production capsules have no IO Drop.
        if let Some(child) = &mut self.child { let _ = child.kill(); let _ = child.wait(); }
    }
}

#[test]
fn actual_pending_and_required_child_barriers_cannot_issue_observed_runtime_identity() {
    let mut fixture=Fixture::new();let start=fixture.start();let helper=start.pending_on_app().unwrap();
    let actual=start.clone();let original=fixture.original.clone();let flag=fixture.cancelled.clone();
    thread::spawn(move||{
        assert!(actual.observe_joined_original_off_app().is_err());
        actual.require_image_off_app(&original,&flag).unwrap();
        assert!(actual.observe_joined_original_off_app().is_err());
        original.cancel();assert!(actual.observe_joined_original_off_app().is_err());
        assert!(original.remaining().is_err());
    }).join().unwrap();
    assert!(fixture.child.as_mut().unwrap().try_wait().unwrap().is_none());
    assert!(Arc::ptr_eq(&start.pending_on_app().unwrap(),&helper));
    let state=start.state.try_lock().unwrap();assert!(state.grant.is_none()&&!state.attached&&state.barrier.is_pending());
    drop(state);drop(start);drop(helper);fixture.finish();
}

#[test]
fn observed_runtime_identity_refuses_same_actual_poisoned_child_carrier() {
    let fixture=Fixture::new();let start=fixture.start();let actual=start.clone();
    assert!(thread::spawn(move||{let _held=actual.state.lock().unwrap();panic!("actual original child carrier panic");}).join().is_err());
    let actual=start.clone();thread::spawn(move||assert!(actual.observe_joined_original_off_app().is_err())).join().unwrap();
    assert!(start.state.is_poisoned());drop(start);fixture.finish();
}

#[test]
fn same_barrier_helper_and_original_cancel_survive_off_app_validation_and_alias_drop() {
    let fixture = Fixture::new(); let start = fixture.start();
    let helper = start.pending_on_app().unwrap(); let alias = Arc::clone(&start);
    let budget = fixture.original.clone(); let constraint = budget.constraint(); let cancelled = fixture.cancelled.clone();
    thread::spawn(move || { alias.check_legacy_off_app(&budget, &cancelled).unwrap(); }).join().unwrap();
    assert_eq!(fixture.original.constraint(), constraint);
    assert!(Arc::ptr_eq(&start.pending_on_app().unwrap(), &helper));
    helper.duplicate_live_pidfd_for_seed_enrollment().unwrap();
    drop(start); drop(helper); fixture.finish();
}

#[test]
fn selected_runtime_requirement_cannot_fall_back_or_release_without_original_attachment() {
    let fixture = Fixture::new(); let start = fixture.start(); let worker = Arc::clone(&start);
    let budget = fixture.original.clone(); let cancelled = fixture.cancelled.clone();
    thread::spawn(move || {
        worker.require_image_off_app(&budget, &cancelled).unwrap();
        assert!(worker.check_legacy_off_app(&budget, &cancelled).is_err());
    }).join().unwrap();
    let helper = start.pending_on_app().unwrap();
    assert!(start.release_after_seed_ack_on_app(&helper).is_err());
    let state = start.state.try_lock().unwrap();
    assert!(state.required && !state.attached && state.grant.is_none() && state.barrier.is_pending());
    drop(state); drop(helper); drop(start); fixture.finish();
}

#[test]
fn copied_constraint_or_new_cancel_arc_cannot_replace_actual_task_carrier() {
    let fixture = Fixture::new(); let start = fixture.start(); let worker = Arc::clone(&start);
    let copied = FinalizationBudget::from_constraint(&fixture.original.constraint()).unwrap();
    thread::spawn(move || {
        assert!(worker.check_legacy_off_app(&copied, &Arc::new(AtomicBool::new(false))).is_err());
    }).join().unwrap();
    fixture.cancelled.store(true, Ordering::Release);
    let worker = Arc::clone(&start); let budget = fixture.original.clone(); let flag = fixture.cancelled.clone();
    thread::spawn(move || { assert!(worker.check_legacy_off_app(&budget, &flag).is_err()); }).join().unwrap();
    assert!(fixture.original.remaining().is_err());
    let state = start.state.try_lock().unwrap(); assert!(state.grant.is_none());
    assert!(Arc::ptr_eq(&state.barrier.pending_helper_process(), &start.helper));
    drop(state); drop(start); fixture.finish();
}

#[test]
fn app_contention_is_nonblocking_and_off_app_cannot_adopt_or_release_barrier() {
    let fixture = Fixture::new(); let start = fixture.start();
    assert!(start.check_legacy_off_app(&fixture.original, &fixture.cancelled).is_err());
    let (entered, wait) = mpsc::sync_channel(1); let (release, released) = mpsc::sync_channel(1);
    let worker = Arc::clone(&start); let budget = fixture.original.clone(); let flag = fixture.cancelled.clone();
    let joined = thread::spawn(move || {
        assert!(worker.pending_on_app().is_err());
        let state = worker.off_app(&budget, &flag).unwrap(); entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap(); drop(state);
    });
    wait.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(matches!(start.state.try_lock(), Err(TryLockError::WouldBlock)));
    assert!(start.pending_on_app().is_err());
    assert!(start.release_after_seed_ack_on_app(&start.helper).is_err());
    release.send(()).unwrap(); joined.join().unwrap();
    assert!(Arc::ptr_eq(&start.pending_on_app().unwrap(), &start.helper));
    drop(start); fixture.finish();
}

#[test]
fn worker_panic_retains_poisoned_original_barrier_instead_of_creating_another() {
    let fixture = Fixture::new(); let start = fixture.start(); let worker = Arc::clone(&start);
    let budget = fixture.original.clone(); let flag = fixture.cancelled.clone();
    assert!(thread::spawn(move || {
        let _held = worker.off_app(&budget, &flag).unwrap();
        panic!("test-only retained barrier panic");
    }).join().is_err());
    assert!(start.pending_on_app().is_err());
    assert!(start.release_after_seed_ack_on_app(&start.helper).is_err());
    let poisoned = start.state.try_lock().err().unwrap();
    assert!(matches!(&poisoned, TryLockError::Poisoned(_)));
    drop(poisoned); drop(start); fixture.finish();
}

#[test]
fn configured_baked_profile_refuses_legacy_environment_before_effects_and_absence_is_unchanged() {
    use rudder_engine::protocol::{ObjectKind, ObjectRef};
    let policy = HostBakedRuntimePolicy { profile: "baked-cas-v1".into(),
        inventory: ObjectRef::from_bytes(ObjectKind::Blob, b"unqualified test-only inventory DATA") };
    let mut effects = 0;
    let result = (|| -> Result<()> { require_legacy_environment_profile(Some(&policy))?; effects += 1; Ok(()) })();
    assert!(result.is_err()); assert_eq!(effects, 0);
    require_legacy_environment_profile(None).unwrap();
    // A misspelled/foreign profile also refuses; DATA cannot select fallback.
    let foreign = HostBakedRuntimePolicy { profile: "not-admitted".into(), inventory: policy.inventory };
    assert!(require_legacy_environment_profile(Some(&foreign)).is_err());
}

#[test]
fn unknown_machine_policy_cannot_select_legacy_or_mint_a_client() {
    let machine = MachineRuntimeImage::new(thread::current().id());
    assert!(machine.selected().is_err());
    assert!(machine.client.is_none() && machine.config.is_none());
}

#[test]
fn actual_undispatched_start_cannot_match_a_joined_runtime_completion() {
    let mut fixture=Fixture::new();let start=fixture.start();
    assert!(start.check_joined_original_off_app().is_err()); // App cannot assert completion.
    let original=start.clone();thread::spawn(move||{
        assert!(original.check_joined_original_off_app().is_err()); // Actual pending barrier.
        let state=original.state.try_lock().unwrap();assert!(state.barrier.is_pending());
        assert!(state.client.is_none()&&state.grant.is_none()&&!state.attached);
    }).join().unwrap();
    assert!(fixture.child.as_mut().unwrap().try_wait().unwrap().is_none());
    drop(start);fixture.finish();
}
