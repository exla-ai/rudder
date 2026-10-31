#![cfg(target_os = "linux")]
//! Explicit local cases: no public DNS/HTTPS and no parent namespace changes.
use rudder_engine::execution::network::connected::ConnectRequest;
use serde_json::Value;
use std::process::Command;

fn fixture(argument: &str) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_rudder-connected-network-qualifier"))
        .arg(argument)
        .env_clear()
        .current_dir("/")
        .output()
        .unwrap();
    assert!(output.status.success(), "owned local qualifier failed");
    assert!(output.stdout.len() <= 8192 && output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn default_description_does_not_activate_network_or_managed_execution() {
    let value = fixture("--describe");
    assert_eq!(value["defaultNetworkEffects"], false);
    assert_eq!(value["managedConnectedActivation"], false);
    assert_eq!(value["transportVersion"], 1);
    let output = Command::new(env!("CARGO_BIN_EXE_rudder-connected-network-qualifier"))
        .env_clear()
        .current_dir("/")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty() && output.stderr.len() < 1024);
}
#[test]
fn actual_scm_rights_roundtrip_keeps_owned_fd_and_cloexec() {
    let value = fixture("--local-ipc-selftest");
    assert_eq!(value["status"], "passed");
    assert_eq!(value["networkEffects"], false);
}
#[test]
fn actual_owned_resolver_style_process_is_pidfd_killed_and_reaped() {
    let value = fixture("--local-process-selftest");
    assert_eq!(value["status"], "passed");
    assert_eq!(value["pidfdKilled"], true);
    assert_eq!(value["actualChildReaped"], true);
    assert_eq!(value["networkEffects"], false);
}
#[test]
fn actual_resolver_process_rejects_oversized_frame_before_any_dns() {
    let value = fixture("--local-resolver-frame-selftest");
    assert_eq!(value["status"], "passed");
    assert_eq!(value["oversizedFrameRejected"], true);
    assert_eq!(value["actualChildReaped"], true);
    assert_eq!(value["dnsQueries"], 0);
    assert_eq!(value["networkEffects"], false);
}
#[test]
fn bounded_header_rejects_pipelining_auth_and_non_https_target() {
    let good = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n";
    assert_eq!(
        ConnectRequest::parse(good, 4096).unwrap().host,
        "example.com"
    );
    for request in [
        "CONNECT example.com:80 HTTP/1.1\r\n\r\n",
        "CONNECT example.com:443 HTTP/1.1\r\nAuthorization: private\r\n\r\n",
        "CONNECT example.com:443 HTTP/1.1\r\n\r\nUser-Agent: after-end\r\n\r\n",
        "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n Host: other\r\n\r\n",
    ] {
        assert!(ConnectRequest::parse(request.as_bytes(), 4096).is_err());
    }
    assert!(ConnectRequest::parse(&vec![b'A'; 4097], 4096).is_err());
}
