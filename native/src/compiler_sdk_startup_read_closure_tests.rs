//! Real socket mechanics and production diagnostic-state tests only. No
//! RuntimeBootstrapProof, SDK process/qualification or result owner is forged.
use super::*;
use std::os::unix::process::ExitStatusExt;

#[test]
fn startup_read_closed_waits_original_exit_and_each_original_pipe_eof() {
    let mut state=StartupResultReadClosure::default();state.observe(true).unwrap();
    let success=ExitStatus::from_raw(0);
    for exit in [None,Some(&success)] {for out in [false,true] {for err in [false,true] {
        let actual=state.poll_original(exit,out,err);
        if exit.is_some()&&out&&err {
            let error=actual.unwrap_err();assert_eq!(bootstrap_failure_check(&error).as_deref(),Some("SdkHelperExitUnknown"));
        } else {assert!(!actual.unwrap());}
    }}}
    assert!(state.observed); // observation never reopens the receive direction
}
#[test]
fn startup_read_closed_never_bypasses_original_nonzero_child_identity_checks() {
    let mut state=StartupResultReadClosure::default();state.observe(true).unwrap();
    let failed=ExitStatus::from_raw(7<<8);
    for out in [false,true] {for err in [false,true] {
        let actual=state.poll_original(Some(&failed),out,err);
        if out&&err {assert!(actual.is_err());} else {assert!(!actual.unwrap());}
    }}
    // Only the existing original failed-child branch may invoke this parser;
    // this observation object cannot issue that original identity/EOF proof.
    let unknown=handshake_diagnostic::failed_original_stderr(b"not a fixed frame\n",true);
    assert_eq!(bootstrap_failure_check(&unknown).as_deref(),Some("SdkHelperExitUnknown"));
}
#[test]
fn read_closed_latch_refuses_ordinary_job_and_unobserved_state() {
    let mut ordinary=StartupResultReadClosure::default();assert!(ordinary.observe(false).is_err());assert!(!ordinary.observed);
    assert!(ordinary.poll_original(None,false,false).is_err());
    ordinary.observe(true).unwrap();assert!(ordinary.observe(false).is_err());assert!(ordinary.observed);
}
#[test]
fn actual_socket_half_close_is_latched_without_adopting_peer_full_closure_or_result() {
    let (left,right)=wire::runtime_socket_pair().unwrap();let alias=right.try_clone().unwrap();
    let left=RuntimeEndpoint::capture(left).unwrap();let right=RuntimeEndpoint::capture(right).unwrap();
    right.shutdown_disposal_write().unwrap();let mut pending=None;
    let read=left.receive_disposal_retaining(&mut pending).unwrap();
    assert!(matches!(read,wire::RuntimeDisposalRead::ReadClosed{peer_fully_closed:false}));
    let mut state=StartupResultReadClosure::default();state.observe(true).unwrap();
    assert!(pending.is_none());assert!(!state.poll_original(None,false,false).unwrap());
    // Keep the SAME endpoint and live peer alias throughout. No second receive,
    // packet, child result, disposal ACK or peer-full-closure fact is fabricated.
    assert!(state.observed);drop(alias);drop(right);drop(left);
}
