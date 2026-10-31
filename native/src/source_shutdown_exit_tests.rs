//! Real child tests, not synthetic PID/status receipt fixtures.
use super::*;

#[test]
fn helper_exit_witness_requires_actual_successful_child_wait() {
    for code in [0, 7] {
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(TerminalCommand::with_args(
                "/bin/sh",
                ["-c", &format!("exit {code}")],
            )),
            TerminalPaneOptions::default(),
        )
        .unwrap();
        let pid = pane.child_process_id().unwrap();
        let custody = pane.retain_managed_helper_custody().unwrap();
        let end = Instant::now() + Duration::from_secs(4);
        loop {
            pane.drain_output();
            match pane.successful_managed_helper_exit() {
                Ok(Some(witness)) => {
                    assert_eq!(code, 0);
                    assert_eq!(witness.pid, pid);
                    assert!(Arc::ptr_eq(&witness.child, &custody.child));
                    assert!(pane.child_process_id().is_none());
                    break;
                }
                Err(_) => {
                    assert_eq!(code, 7);
                    assert!(pane.exit_observed());
                    break;
                }
                Ok(None) => {
                    assert!(Instant::now() < end, "actual child did not settle");
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

#[test]
fn replacement_pane_cannot_recreate_initial_owned_child_custody() {
    fn pane() -> TerminalPane {
        TerminalPane::spawn_shell_or_command(
            Some(TerminalCommand::with_args(
                "/bin/sh",
                ["-c", "read -r line"],
            )),
            TerminalPaneOptions::default(),
        )
        .unwrap()
    }
    let original = pane();
    let retained = original.retain_managed_helper_custody().unwrap();
    let replacement = pane();
    let other = replacement.retain_managed_helper_custody().unwrap();
    assert!(!Arc::ptr_eq(&retained.child, &other.child));
}

#[test]
fn interrupt_request_and_live_child_cannot_create_helper_exit_witness() {
    let mut pane = TerminalPane::spawn_shell_or_command(
        Some(TerminalCommand::with_args(
            "/bin/sh",
            ["-c", "read -r line"],
        )),
        TerminalPaneOptions::default(),
    )
    .unwrap();
    assert!(pane.successful_managed_helper_exit().unwrap().is_none());
    pane.request_termination();
    let end = Instant::now() + Duration::from_secs(4);
    loop {
        pane.drain_output();
        match pane.successful_managed_helper_exit() {
            Ok(Some(_)) => panic!("termination request cannot certify successful helper exit"),
            Err(_) => break,
            Ok(None) => {
                assert!(Instant::now() < end, "terminated child did not settle");
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
