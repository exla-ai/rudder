//! Actual pending helper/thread custody. No Root enrollment is fabricated.
use super::*;
fn handoff()->ManagedExecutionHandoff{
    let mut binding=readiness_tests::binding();binding.format_version=2;binding.profile=None;binding.execution_profile="claude-sdk".into();binding.frontend_start=ManagedFrontendStart::Immediate;binding.server_command=None;
    let name=format!("rudder-execution-{}",Uuid::new_v4());let listener=UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();listener.set_nonblocking(true).unwrap();let(parent_control,helper_control)=UnixStream::pair().unwrap();
    ManagedExecutionHandoff{handoff:ExecutionHandoff{listener,spec:super::super::linux::sealed_bytes_fd(b"benign staged FD fixture").unwrap(),name,parent:std::process::id(),uid:unsafe{libc::geteuid()}},binding:binding.clone(),binding_fd:super::super::linux::sealed_bytes_fd(&crate::protocol::canonical_json(&binding).unwrap()).unwrap(),parent_control,helper_control,request_digest:"1".repeat(64)}
}
struct Child(std::process::Child);
impl Drop for Child{fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
#[test]
fn actual_staged_barrier_keeps_original_cancel_and_runtime_requirement_without_dispatch(){
    let mut child=Child(std::process::Command::new("/bin/sleep").arg("30").env_clear().stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap());
    let original=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();let cancelled=Arc::new(std::sync::atomic::AtomicBool::new(false));let mut activation=RetainedManagedExecutionActivation::new(handoff());activation.stage_bounded(child.0.id(),&original,cancelled.clone()).unwrap();let mut barrier=activation.take_start_barrier().unwrap();
    let(actual,flag)=barrier.original_request_retaining().unwrap();assert_eq!(actual.constraint(),original.constraint());assert!(Arc::ptr_eq(&flag,&cancelled));
    barrier.require_runtime_image_before_release();assert!(barrier.release_after_enrollment().unwrap_err().to_string().contains("runtime original enrollment not attached"));
    cancelled.store(true,std::sync::atomic::Ordering::Release);assert!(barrier.original_request_retaining().is_err());assert!(original.remaining().is_err());assert!(!activation.dispatch_attempted.load(std::sync::atomic::Ordering::Acquire));
    // A decoded restriction cannot replace the actual cancellation carrier.
    let decoded=FinalizationBudget::from_constraint(&original.constraint()).unwrap();assert!(decoded.remaining().is_ok());assert!(barrier.original_request_retaining().is_err());
    drop(barrier);let cleanup=FinalizationBudget::begin(Duration::from_secs(30)).unwrap();activation.settle_undispatched_off_app(&cleanup).unwrap();assert!(activation.is_settled());
    // Actual worker join did NOT establish actual helper/process exit.
    assert!(child.0.try_wait().unwrap().is_none());child.0.kill().unwrap();assert!(!child.0.wait().unwrap().success());
}
