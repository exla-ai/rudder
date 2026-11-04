//! Actual sockets, sealed FDs, pidfds and child processes. These tests exercise
//! descriptor custody only; the benign descriptor recipient is not a backend.
use super::*;
use std::{cell::Cell, process::{Command, Stdio}, sync::atomic::{AtomicBool, Ordering}};
thread_local! { static REFUSE:Cell<bool>=const { Cell::new(false) }; }
pub(super) fn refuse_spawn()->bool { REFUSE.with(|flag|flag.replace(false)) }
const CHILD:&str="execution::handoff::retained_activation_tests::descriptor_child";
const ROUTE:&str="RUDDER_RETAINED_ACTIVATION_TEST_ROUTE";
fn budget()->FinalizationBudget { FinalizationBudget::begin(Duration::from_secs(3)).unwrap() }
fn handoff()->ManagedExecutionHandoff {
    // Private test fixture: real sealed descriptors and original listener. It
    // intentionally does not mint a validated execution/helper/runtime witness.
    let mut binding=readiness_tests::binding();binding.format_version=2;binding.profile=None;
    binding.execution_profile="claude-sdk".into();binding.frontend_start=ManagedFrontendStart::Immediate;
    binding.server_command=None;
    let name=format!("rudder-execution-{}",Uuid::new_v4());
    let listener=UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();let (parent_control,helper_control)=UnixStream::pair().unwrap();
    ManagedExecutionHandoff { handoff:ExecutionHandoff { listener,spec:super::super::linux::sealed_bytes_fd(b"benign descriptor fixture").unwrap(),
        name,parent:std::process::id(),uid:unsafe { libc::geteuid() } },binding:binding.clone(),
        binding_fd:super::super::linux::sealed_bytes_fd(&crate::protocol::canonical_json(&binding).unwrap()).unwrap(),
        parent_control,helper_control,request_digest:"a".repeat(64) }
}
struct Child(std::process::Child);
impl Drop for Child { fn drop(&mut self) { let _=self.0.kill();let _=self.0.wait(); } }
fn sleeper()->Child { Child(Command::new("/bin/sleep").arg("30").spawn().unwrap()) }
#[test]
fn activation_validation_error_keeps_original_descriptors_in_the_caller() {
    let mut owner=RetainedManagedExecutionActivation::new(handoff());
    let fd=owner.original.lock().unwrap().as_ref().unwrap().handoff.spec.as_raw_fd();
    assert!(owner.activate(0).is_err());
    assert!(unsafe { libc::fcntl(fd,libc::F_GETFD) }>=0);
    assert!(owner.original.lock().unwrap().is_some());assert!(owner.live.is_none());
    owner.settle_undispatched_off_app(&budget()).unwrap();assert!(owner.is_settled());
}
#[test]
fn control_spawn_refusal_retains_actual_handoff_and_pidfd_until_unused_settlement() {
    let mut child=sleeper();let mut owner=RetainedManagedExecutionActivation::new(handoff());
    let fd=owner.original.lock().unwrap().as_ref().unwrap().binding_fd.as_raw_fd();
    REFUSE.with(|flag|flag.set(true));assert!(owner.activate(child.0.id()).is_err());
    assert!(owner.lifetime.is_some());assert!(owner.original.lock().unwrap().is_some());
    assert!(unsafe { libc::fcntl(fd,libc::F_GETFD) }>=0);assert!(child.0.try_wait().unwrap().is_none());
    child.0.kill().unwrap();child.0.wait().unwrap();
    owner.settle_undispatched_off_app(&budget()).unwrap();assert!(owner.is_settled());
}
#[test]
fn closed_unconnected_worker_must_actually_join_before_proving_no_dispatch() {
    let mut child=sleeper();let mut owner=RetainedManagedExecutionActivation::new(handoff());
    owner.activate(child.0.id()).unwrap();owner.close();child.0.kill().unwrap();child.0.wait().unwrap();
    owner.settle_undispatched_off_app(&budget()).unwrap();
    assert!(owner.control_join.is_some());assert!(owner.receipt_join.is_some());
    assert!(owner.control_join.as_ref().unwrap().same_worker(owner.receipt_join.as_ref().unwrap()));
    assert!(!owner.dispatch_attempted.load(Ordering::Acquire));assert!(owner.is_settled());
}
#[test]
fn original_cancel_and_expiry_refuse_descriptor_worker_without_budget_renewal() {
    for expired in [false,true] {
        let mut child=sleeper();let mut owner=RetainedManagedExecutionActivation::new(handoff());
        let original=FinalizationBudget::begin(Duration::from_millis(15)).unwrap();let constraint=original.constraint();
        let cancelled=Arc::new(AtomicBool::new(!expired));
        if expired { std::thread::sleep(Duration::from_millis(25)); }
        assert!(owner.activate_bounded(child.0.id(),&original,cancelled).is_err());
        assert_eq!(original.constraint(),constraint);assert!(owner.live.is_none());
        assert!(owner.original.lock().unwrap().is_some());
        child.0.kill().unwrap();child.0.wait().unwrap();owner.settle_undispatched_off_app(&budget()).unwrap();
    }
}
#[test]
#[ignore = "private benign descriptor recipient, invoked only by the owning test"]
fn descriptor_child() {
    let route=std::env::var(ROUTE).expect("owned descriptor test route");
    let mut stream=UnixStream::connect_addr(&SocketAddr::from_abstract_name(route.as_bytes()).unwrap()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let (tag,descriptors)=receive_descriptors(&stream).unwrap();assert_eq!(tag,1);assert_eq!(descriptors.len(),3);
    // Retain every original descriptor until the parent closes its owned pipe.
    let mut byte=[0];let _=std::io::stdin().read(&mut byte);drop(descriptors);let _=stream.flush();
}
#[test]
fn actual_descriptor_exposure_never_becomes_unused_from_helper_exit_or_control_join() {
    let handoff=handoff();let route=handoff.handoff.name.clone();
    let mut child=Child(Command::new(std::env::current_exe().unwrap()).args(["--exact",CHILD,"--ignored","--nocapture"])
        .env(ROUTE,route).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let mut owner=RetainedManagedExecutionActivation::new(handoff);owner.activate(child.0.id()).unwrap();
    owner.live.as_ref().unwrap().0.wait(Duration::from_secs(3)).unwrap();
    assert!(owner.dispatch_attempted.load(Ordering::Acquire));
    drop(child.0.stdin.take());child.0.wait().unwrap();
    assert!(owner.settle_undispatched_off_app(&budget()).is_err());
    assert!(owner.control_join.is_some());assert!(owner.receipt_join.is_some());
    assert!(!owner.is_settled(),"actual helper exit/worker join cannot erase possible backend effects");
}
#[test]
fn staged_original_helper_receives_no_descriptors_until_explicit_start_release() {
    let handoff=handoff();let route=handoff.handoff.name.clone();
    let mut child=Child(Command::new(std::env::current_exe().unwrap()).args(["--exact",CHILD,"--ignored","--nocapture"])
        .env(ROUTE,route).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let mut owner=RetainedManagedExecutionActivation::new(handoff);
    let original=budget();owner.stage_bounded(child.0.id(),&original,Arc::new(AtomicBool::new(false))).unwrap();
    let mut barrier=owner.take_start_barrier().unwrap();let pending=barrier.pending_helper_process();
    assert!(Arc::ptr_eq(&pending,&owner.live.as_ref().unwrap().1.helper_process_witness()));
    assert!(pending.check_current().is_err(),"pending kernel custody is not readiness");
    std::thread::sleep(Duration::from_millis(30));
    assert!(!owner.dispatch_attempted.load(Ordering::Acquire));assert!(owner.live.as_ref().unwrap().0.try_result().is_none());
    // Protocol fixture exercises the release capability only. No real Root seed
    // enrollment is asserted; production must supply that separate typed ACK.
    barrier.release_after_enrollment().unwrap();
    owner.live.as_ref().unwrap().0.wait(Duration::from_secs(3)).unwrap();
    assert!(!barrier.is_pending());assert!(owner.dispatch_attempted.load(Ordering::Acquire));
    drop(child.0.stdin.take());child.0.wait().unwrap();
    assert!(owner.settle_undispatched_off_app(&budget()).is_err());
}
#[test]
fn dropping_closed_start_barrier_cancels_actual_worker_without_exposing_descriptors() {
    let mut child=sleeper();let mut owner=RetainedManagedExecutionActivation::new(handoff());
    owner.stage_bounded(child.0.id(),&budget(),Arc::new(AtomicBool::new(false))).unwrap();
    let barrier=owner.take_start_barrier().unwrap();drop(barrier);
    child.0.kill().unwrap();child.0.wait().unwrap();owner.settle_undispatched_off_app(&budget()).unwrap();
    assert!(owner.is_settled());assert!(!owner.dispatch_attempted.load(Ordering::Acquire));
}
