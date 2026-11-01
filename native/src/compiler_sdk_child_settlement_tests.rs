//! Harmless real child/pipe component cases; no SDK owner or Root ACK fake.
use super::*;
use std::process::{Command,Stdio};
fn carrier(script:&str)->ChildSettlement{let mut child=Command::new("/bin/sh").args(["-c",script]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stdout=child.stdout.take();let mut stderr=child.stderr.take();let mut child=Some(child);let mut out=None;ChildSettlement::stage_retaining(&mut out,&mut child,&mut stdout,&mut stderr).unwrap();out.unwrap()}
#[test]fn sdk_job_component_reaps_original_child_and_both_actual_pipes(){let mut actual=carrier("printf output; printf diagnostic >&2");let pid=actual.child.as_ref().unwrap().id();let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();
    actual.run_retaining(&original).unwrap();assert_eq!(actual.original_pid_data().unwrap(),pid);assert!(actual.child.is_none()&&actual.stdout.is_none()&&actual.stderr.is_none());assert_eq!(actual.stdout_bytes,6);assert_eq!(actual.stderr_bytes,10);
    // ECHILD is a kernel check on the actual already-waited original child.
    assert_eq!(unsafe{libc::waitpid(pid as i32,std::ptr::null_mut(),libc::WNOHANG)},-1);assert_eq!(io::Error::last_os_error().raw_os_error(),Some(libc::ECHILD));}
#[test]fn sdk_job_component_cancelled_clock_retains_the_original_live_child_and_pipes(){let mut actual=carrier("read value");let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();original.cancel();
    assert!(actual.run_retaining(&original).is_err());assert!(actual.child.is_some()&&actual.stdout.is_some()&&actual.stderr.is_some());assert!(actual.original_pid_data().is_err());
    // Explicit fixture cleanup, not production settlement or process-tree proof.
    actual.child.as_mut().unwrap().kill().unwrap();actual.child.as_mut().unwrap().wait().unwrap();}
#[test]fn sdk_job_component_nonzero_original_exit_never_becomes_a_resource_ack(){let mut actual=carrier("printf failed >&2; exit 7");let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();
    assert!(actual.run_retaining(&original).is_err());assert!(actual.original_pid_data().is_err());assert!(actual.child.is_some()&&actual.stdout.is_some()&&actual.stderr.is_some());actual.child.as_mut().unwrap().wait().unwrap();}
