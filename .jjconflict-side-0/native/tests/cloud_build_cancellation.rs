//! Signal disposition changes happen only inside an actual disposable child.
//! Include the production CLI so this narrow target also compiles its wiring.
#![cfg(target_os = "linux")]
#[path = "../src/cloud_build_profile_cli.rs"]
mod cli;
#[path = "../src/cloud_build_cancel.rs"]
mod cancellation;

use rudder_engine::build::BuildExecutionControl;
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

struct OriginalChild(Child);
impl Drop for OriginalChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn signal_child(signal: libc::c_int, case: &str) {
    let mut child = OriginalChild(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "original_cancellation_child", "--nocapture"])
        .env("RUDDER_BUILD_CANCEL_TEST_CASE", case)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap());
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let mut text = String::new();
    loop {
        let mut line = String::new();
        assert!(output.read_line(&mut line).unwrap() > 0, "child exited before original owner installed: {text}");
        text.push_str(&line);
        if line.contains("original cancellation owner ready") { break; }
    }
    assert_eq!(unsafe { libc::kill(child.0.id() as libc::pid_t, signal) }, 0);
    child.0.stdin.take().unwrap().write_all(b"finish\n").unwrap();
    output.read_to_string(&mut text).unwrap();
    let status = child.0.wait().unwrap();
    let mut stderr = String::new();
    child.0.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(status.success(), "actual original signal child failed: {status}; {text}; {stderr}");
    assert!(text.contains("original cancellation owner settled"), "{text}");
}

#[test]
fn sigterm_cancels_original_control_and_joins_its_monitor() {
    signal_child(libc::SIGTERM, "finish");
}

#[test]
fn sigint_cancels_original_control_and_joins_its_monitor() {
    signal_child(libc::SIGINT, "finish");
}

#[test]
fn early_return_drops_and_joins_original_monitor_before_restoring_signals() {
    signal_child(libc::SIGTERM, "drop");
}

#[test]
fn original_cancellation_child() {
    let Ok(case) = std::env::var("RUDDER_BUILD_CANCEL_TEST_CASE") else { return; };
    let mut before: libc::sigaction = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut before) }, 0);
    let original = Arc::new(BuildExecutionControl::new(Duration::from_secs(10)).unwrap());
    let owner = cancellation::BuildCancellation::install(original.clone()).unwrap();
    // The caller, guard and actual monitor all retain this same original Arc.
    assert_eq!(Arc::strong_count(&original), 3);
    assert!(cancellation::BuildCancellation::install(Arc::new(BuildExecutionControl::new(Duration::from_secs(10)).unwrap())).is_err());
    println!("original cancellation owner ready");
    std::io::stdout().flush().unwrap();
    let mut handshake = String::new();
    std::io::stdin().read_line(&mut handshake).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Arc::strong_count(&original) == 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(Arc::strong_count(&original), 2, "actual original cancellation monitor did not return");
    assert!(owner.check().is_err());
    if case == "finish" {
        let report = owner.finish().unwrap();
        assert!(report.requested && report.publication_prevented);
    } else {
        assert_eq!(case, "drop");
        drop(owner);
    }
    assert_eq!(Arc::strong_count(&original), 1, "original monitor not joined/released");
    let mut after: libc::sigaction = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut after) }, 0);
    assert_eq!(before.sa_sigaction, after.sa_sigaction);
    // No stale flag or leaked owner may cancel a later invocation.
    let later = Arc::new(BuildExecutionControl::new(Duration::from_secs(10)).unwrap());
    let next = cancellation::BuildCancellation::install(later.clone()).unwrap();
    next.check().unwrap();
    let report = next.finish().unwrap();
    assert!(!report.requested && !report.publication_prevented);
    assert_eq!(Arc::strong_count(&later), 1);
    println!("original cancellation owner settled");
}
