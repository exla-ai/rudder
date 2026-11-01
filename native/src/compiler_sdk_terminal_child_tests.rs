//! Real kernel Child/pipe tests only. No SDK/Root/canonical grant is faked.
use super::*;
use std::{process::{Command,Stdio},io::Write};
fn child(script:&str)->ChildSettlement{let mut child=Command::new("/bin/sh").args(["-c",script]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut out=None;let mut stdout=child.stdout.take();let mut stderr=child.stderr.take();let mut child=Some(child);ChildSettlement::stage_retaining(&mut out,&mut child,&mut stdout,&mut stderr).unwrap();out.unwrap()}
#[test]fn known_failed_original_child_reaps_both_real_pipes_without_a_success_receipt(){let mut actual=child("printf partial; printf 'actual helper diagnostic' >&2; exit 7");let pid=actual.child.as_ref().unwrap().id();
    let original=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();actual.run_terminal_failure_retaining(&original).unwrap();assert_eq!(actual.original_failed_pid_data().unwrap(),pid);assert!(actual.original_pid_data().is_err());
    assert!(actual.child.is_none()&&actual.stdout.is_none()&&actual.stderr.is_none());assert_eq!(actual.stdout_bytes,7);assert_eq!(actual.stderr_bytes,24);
    assert_eq!(unsafe{libc::waitpid(pid as i32,std::ptr::null_mut(),libc::WNOHANG)},-1);assert_eq!(io::Error::last_os_error().raw_os_error(),Some(libc::ECHILD));}
#[test]fn successful_original_child_cannot_mint_a_terminal_failure_disposal(){let mut actual=child("exit 0");assert!(actual.run_terminal_failure_retaining(&FinalizationBudget::begin(Duration::from_secs(5)).unwrap()).is_err());
    assert!(actual.original_failed_pid_data().is_err()&&actual.original_pid_data().is_err());assert!(actual.child.is_some());actual.child.as_mut().unwrap().wait().unwrap();}
#[test]fn cancelled_failed_cleanup_retains_original_and_cannot_restart_on_new_clock(){let mut actual=child("exit 3");let clock=FinalizationBudget::begin(Duration::from_secs(5)).unwrap();clock.cancel();
    assert!(actual.run_terminal_failure_retaining(&clock).is_err());assert!(actual.child.is_some()&&actual.stdout.is_some()&&actual.stderr.is_some());
    assert!(actual.run_terminal_failure_retaining(&FinalizationBudget::begin(Duration::from_secs(5)).unwrap()).is_err());assert!(actual.original_failed_pid_data().is_err());actual.child.as_mut().unwrap().wait().unwrap();}
#[test]fn failed_wait_alone_does_not_settle_a_pipe_still_held_by_an_actual_child(){
    // The original shell really exits nonzero while its child holds stdout.
    // FD3 retains a deterministic release gate even with async stdin=/dev/null.
    let mut actual=child("exec 3<&0; (read value <&3; printf held) & exit 4");let mut stdin=actual.child.as_mut().unwrap().stdin.take().unwrap();
    actual.exit=Some(actual.child.as_mut().unwrap().wait().unwrap());assert!(!actual.exit.unwrap().success());assert!(!actual.stdout_eof);
    let(entered,started)=std::sync::mpsc::sync_channel(1);let(done,wait)=std::sync::mpsc::sync_channel(1);
    let joined=std::thread::spawn(move||{entered.send(()).unwrap();let result=actual.run_terminal_failure_retaining(&FinalizationBudget::begin(Duration::from_secs(5)).unwrap());done.send(result.is_ok()).unwrap();actual});
    started.recv_timeout(Duration::from_secs(5)).unwrap();assert!(matches!(wait.recv_timeout(Duration::from_millis(50)),Err(std::sync::mpsc::RecvTimeoutError::Timeout)));
    stdin.write_all(b"release\n").unwrap();assert!(wait.recv_timeout(Duration::from_secs(5)).unwrap());let actual=joined.join().unwrap();assert!(actual.original_failed_pid_data().is_ok());assert_eq!(actual.stdout_bytes,4);
}
