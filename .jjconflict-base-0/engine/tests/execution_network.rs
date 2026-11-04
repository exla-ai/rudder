use rudder_engine::execution::network::*;
use serde_json::{json, Value};
use std::net::Ipv4Addr;

const REQUEST: &str = "00000000-0000-0000-0000-000000000001";
const EXECUTION: &str = "00000000-0000-0000-0000-000000000002";
const OPERATION: &str = "00000000-0000-0000-0000-000000000003";
const VIEW: &str = "00000000-0000-0000-0000-000000000004";
const NETWORK: &str = "00000000-0000-0000-0000-000000000005";
const NONCE: &str = "00000000-0000-0000-0000-000000000006";

fn expected() -> ExpectedNetworkBinding<'static> {
    ExpectedNetworkBinding {
        project_id: "project",
        owner: "engine",
        owner_epoch: 7,
        run_id: "run",
        run_epoch: 2,
        request_id: REQUEST,
        execution_id: EXECUTION,
        operation_id: OPERATION,
        view_id: VIEW,
        network_id: NETWORK,
        readiness_nonce: NONCE,
    }
}
fn host() -> HostIpv4DenySet {
    HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::new(172, 19, 0, 2)]).unwrap()
}
fn offline_wire() -> Value {
    json!({"format_version":1,"execution_profile":"cloud_uid1500_linux_v1",
        "binding":{"project_id":"project","owner":"engine","owner_epoch":"7","run_id":"run","run_epoch":"2",
            "request_id":REQUEST,"execution_id":EXECUTION,"operation_id":OPERATION,"view_id":VIEW,"network_id":NETWORK},
        "mode":"offline_v1","policy":null,"policy_sha256":null,"readiness_nonce":NONCE,
        "budgets":{"max_connections":0,"max_dns_jobs":0,"max_request_bytes":0,"max_buffer_bytes":0,
            "max_bytes_per_connection":0,"max_total_bytes":0,"connect_timeout_ms":0,"idle_timeout_ms":0,"total_timeout_ms":0}})
}
fn qualification_wire() -> Value {
    let mut value = offline_wire();
    value["mode"] = json!("qualification_https_connect_v1");
    value["policy"] = json!({"format_version":1,"origins":[
        {"host":"a.example.com","ipv4_addresses":["1.1.1.1","1.1.1.2"],"port":443},
        {"host":"b.example.com","ipv4_addresses":["8.8.8.8"],"port":443}]});
    value["budgets"] = json!({"max_connections":2,"max_dns_jobs":1,"max_request_bytes":1024,"max_buffer_bytes":8192,
        "max_bytes_per_connection":1024*1024,"max_total_bytes":2*1024*1024,
        "connect_timeout_ms":1000,"idle_timeout_ms":5000,"total_timeout_ms":10000});
    digest(&mut value);
    value
}
fn digest(value: &mut Value) {
    let policy: PublicIpv4HttpsPolicy = serde_json::from_value(value["policy"].clone()).unwrap();
    value["policy_sha256"] = json!(policy.sha256().unwrap());
}
// Negative semantic-policy cases carry a correct independent digest so that a
// digest mismatch cannot accidentally stand in for the boundary under test.
fn seal_unchecked_policy(value: &mut Value) {
    use sha2::{Digest, Sha256};
    let policy: PublicIpv4HttpsPolicy = serde_json::from_value(value["policy"].clone()).unwrap();
    value["policy_sha256"] = json!(hex::encode(Sha256::digest(
        serde_json::to_vec(&policy).unwrap()
    )));
}
fn parse(value: &Value) -> NetworkResult<ValidatedNetworkProfile> {
    NetworkProfileV1::from_json(
        &serde_json::to_vec(value).unwrap(),
        &expected(),
        Some(&host()),
    )
}

#[test]
fn explicit_offline_and_qualification_profiles_are_configuration_only() {
    let offline = parse(&offline_wire()).unwrap();
    assert_eq!(offline.mode(), NetworkMode::OfflineV1);
    assert_eq!(
        offline.execution_profile(),
        NetworkExecutionProfile::CloudUid1500LinuxV1
    );
    assert_eq!(offline.binding().owner_epoch.value(), 7);
    assert_eq!(offline.network_id().to_string(), NETWORK);
    assert!(offline
        .check_destination("a.example.com", 443, "1.1.1.1".parse().unwrap(), &host())
        .is_err());
    let profile = parse(&qualification_wire()).unwrap();
    let checked = profile
        .check_destination("a.example.com", 443, "1.1.1.1".parse().unwrap(), &host())
        .unwrap();
    assert_eq!(checked.address(), Ipv4Addr::new(1, 1, 1, 1));
    assert_eq!(checked.port(), 443);
    assert!(!format!("{profile:?}").contains(NONCE));
}

#[test]
fn bounded_wire_rejects_unknown_duplicate_and_unsupported_fields() {
    let original = offline_wire();
    for (key, value) in [
        ("format_version", json!(2)),
        ("execution_profile", json!("linux")),
        ("mode", json!("packet")),
        ("extra", json!(true)),
        ("readiness_nonce", json!("")),
    ] {
        let mut wire = original.clone();
        wire[key] = value;
        assert!(parse(&wire).is_err(), "{key}");
    }
    for pointer in ["binding", "budgets"] {
        let mut wire = original.clone();
        wire[pointer]["extra"] = json!(true);
        assert!(parse(&wire).is_err());
    }
    let encoded = serde_json::to_string(&original).unwrap();
    for (needle, duplicate) in [
        (
            "\"format_version\":1",
            "\"format_version\":1,\"format_version\":1",
        ),
        (
            "\"owner\":\"engine\"",
            "\"owner\":\"engine\",\"owner\":\"engine\"",
        ),
    ] {
        let wire = encoded.replacen(needle, duplicate, 1);
        assert!(NetworkProfileV1::from_json(wire.as_bytes(), &expected(), Some(&host())).is_err());
    }
    let mut oversized = encoded.into_bytes();
    oversized.resize(MAX_NETWORK_PROFILE_BYTES + 1, b' ');
    assert!(NetworkProfileV1::from_json(&oversized, &expected(), None).is_err());
    assert!(NetworkProfileV1::from_json(b"", &expected(), None).is_err());
}

#[test]
fn all_binding_fields_and_readiness_nonce_must_match_trusted_expected_data() {
    let original = offline_wire();
    for key in ["project_id", "owner", "run_id"] {
        let mut wire = original.clone();
        wire["binding"][key] = json!("other");
        assert_eq!(
            parse(&wire).unwrap_err().kind(),
            NetworkErrorKind::BindingMismatch
        );
    }
    for key in [
        "request_id",
        "execution_id",
        "operation_id",
        "view_id",
        "network_id",
    ] {
        let mut wire = original.clone();
        wire["binding"][key] = json!(NONCE);
        assert_eq!(
            parse(&wire).unwrap_err().kind(),
            NetworkErrorKind::BindingMismatch
        );
    }
    for key in ["owner_epoch", "run_epoch"] {
        let mut wire = original.clone();
        wire["binding"][key] = json!("3");
        assert_eq!(
            parse(&wire).unwrap_err().kind(),
            NetworkErrorKind::BindingMismatch
        );
    }
    let mut wire = original.clone();
    wire["readiness_nonce"] = json!(NETWORK);
    assert_eq!(
        parse(&wire).unwrap_err().kind(),
        NetworkErrorKind::BindingMismatch
    );
    for invalid in ["", " leading", "trailing ", "line\nbreak", "nul\0value"] {
        let mut wire = original.clone();
        wire["binding"]["owner"] = json!(invalid);
        assert_eq!(
            parse(&wire).unwrap_err().kind(),
            NetworkErrorKind::InvalidProfile
        );
    }
    for invalid in [
        "00000000-0000-0000-0000-00000000000A",
        "00000000-0000-0000-0000-000000000000",
        "uuid",
    ] {
        let mut wire = original.clone();
        wire["binding"]["network_id"] = json!(invalid);
        assert!(parse(&wire).is_err());
    }
}

#[test]
fn epoch_decimal_wire_rejects_numbers_aliases_and_signed_overflow() {
    for value in ["1", "9223372036854775807"] {
        let epoch: NetworkEpoch = serde_json::from_value(json!(value)).unwrap();
        assert_eq!(serde_json::to_value(epoch).unwrap(), json!(value));
    }
    for value in [
        json!("0"),
        json!("01"),
        json!("+1"),
        json!("-1"),
        json!("1e1"),
        json!(" 1"),
        json!("9223372036854775808"),
        json!("18446744073709551615"),
        json!(1),
        json!(1.0),
        json!(null),
    ] {
        assert!(serde_json::from_value::<NetworkEpoch>(value).is_err());
    }
    assert!(NetworkEpoch::new(0).is_err());
    assert!(NetworkEpoch::new(u64::MAX).is_err());
}

#[test]
fn offline_requires_no_policy_and_every_egress_budget_zero() {
    let original = offline_wire();
    for key in original["budgets"].as_object().unwrap().keys() {
        let mut wire = original.clone();
        wire["budgets"][key] = json!(1);
        assert_eq!(
            parse(&wire).unwrap_err().kind(),
            NetworkErrorKind::InvalidBudgets
        );
    }
    for key in ["policy", "policy_sha256"] {
        let mut wire = original.clone();
        wire[key] = qualification_wire()[key].clone();
        assert_eq!(
            parse(&wire).unwrap_err().kind(),
            NetworkErrorKind::InvalidPolicy
        );
    }
}

#[test]
fn qualification_budget_limits_and_overflow_fail_closed() {
    let original = qualification_wire();
    for (key, values) in [
        ("max_connections", vec![json!(0), json!(9), json!(65536)]),
        ("max_dns_jobs", vec![json!(0), json!(5)]),
        ("max_request_bytes", vec![json!(255), json!(16385)]),
        ("max_buffer_bytes", vec![json!(4095), json!(262145)]),
        (
            "max_bytes_per_connection",
            vec![json!(0), json!(67108865), json!(u64::MAX)],
        ),
        (
            "max_total_bytes",
            vec![json!(1), json!(268435457), json!(u64::MAX)],
        ),
        ("connect_timeout_ms", vec![json!(0), json!(10001)]),
        ("idle_timeout_ms", vec![json!(0), json!(30001)]),
        (
            "total_timeout_ms",
            vec![json!(0), json!(120001), json!(999)],
        ),
    ] {
        for value in values {
            let mut wire = original.clone();
            wire["budgets"][key] = value;
            assert!(parse(&wire).is_err(), "{key}");
        }
    }
    let mut wire = original.clone();
    wire["budgets"]["max_total_bytes"] = json!(2097153);
    assert_eq!(
        parse(&wire).unwrap_err().kind(),
        NetworkErrorKind::InvalidBudgets
    );
    let mut wire = original;
    wire["budgets"]["total_timeout_ms"] = json!(4999);
    assert!(parse(&wire).is_err());
    let mut wire = qualification_wire();
    wire["budgets"]["max_request_bytes"] = json!(8192);
    wire["budgets"]["max_buffer_bytes"] = json!(4096);
    assert_eq!(
        parse(&wire).unwrap_err().kind(),
        NetworkErrorKind::InvalidBudgets
    );
}

#[test]
fn full_profile_digest_separates_policy_and_budget_under_the_same_network_label() {
    let original = qualification_wire();
    let before = parse(&original).unwrap();
    let mut wire = original.clone();
    wire["budgets"]["connect_timeout_ms"] = json!(999);
    let changed = parse(&wire).unwrap();
    assert_eq!(before.network_id(), changed.network_id());
    assert_ne!(before.profile_digest(), changed.profile_digest());
    let mut wire = original;
    wire["policy"]["origins"][0]["ipv4_addresses"] = json!(["1.1.1.3"]);
    digest(&mut wire);
    assert_ne!(
        before.profile_digest(),
        parse(&wire).unwrap().profile_digest()
    );
}

#[test]
fn canonical_policy_digest_binds_origins_addresses_and_order() {
    let original = qualification_wire();
    let policy: PublicIpv4HttpsPolicy = serde_json::from_value(original["policy"].clone()).unwrap();
    assert_eq!(String::from_utf8(policy.canonical_bytes().unwrap()).unwrap(),
        "{\"format_version\":1,\"origins\":[{\"host\":\"a.example.com\",\"ipv4_addresses\":[\"1.1.1.1\",\"1.1.1.2\"],\"port\":443},{\"host\":\"b.example.com\",\"ipv4_addresses\":[\"8.8.8.8\"],\"port\":443}]}");
    for pointer in ["host", "ipv4_addresses"] {
        let mut wire = original.clone();
        wire["policy"]["origins"][0][pointer] = if pointer == "host" {
            json!("c.example.com")
        } else {
            json!(["1.1.1.3"])
        };
        assert!(parse(&wire).is_err());
    }
    let mut wire = original.clone();
    wire["policy"]["origins"][0]["ipv4_addresses"] = json!(["1.1.1.2", "1.1.1.1"]);
    seal_unchecked_policy(&mut wire);
    assert!(parse(&wire).is_err());
    let mut wire = original.clone();
    wire["policy"]["origins"][1] = wire["policy"]["origins"][0].clone();
    seal_unchecked_policy(&mut wire);
    assert!(parse(&wire).is_err());
    let mut wire = original;
    wire["policy_sha256"] = json!("0".repeat(64));
    assert_eq!(
        parse(&wire).unwrap_err().kind(),
        NetworkErrorKind::PolicyDigestMismatch
    );
}

#[test]
fn hostname_port_and_numeric_aliases_cannot_change_a_qualification_origin() {
    let original = qualification_wire();
    for host in [
        "localhost",
        "a.localhost",
        "a.internal",
        "a.local",
        "a.test",
        "A.example.com",
        "a.example.com.",
        "127.0.0.1",
        "2130706433",
        "http://a.example.com",
        "a.example.com:443",
        "user@a.example.com",
        "a%2eexample.com",
        "[::1]",
        "a..example.com",
        "-a.example.com",
        "a-.example.com",
        "a.example.com/path",
    ] {
        let mut wire = original.clone();
        wire["policy"]["origins"][0]["host"] = json!(host);
        seal_unchecked_policy(&mut wire);
        assert_eq!(
            parse(&wire).unwrap_err().kind(),
            NetworkErrorKind::InvalidPolicy,
            "{host}"
        );
    }
    for address in [
        "127.0.0.1",
        "0177.0.0.1",
        "127.1",
        "0x7f000001",
        "::1",
        "::ffff:1.1.1.1",
        "64:ff9b::101:101",
    ] {
        let mut wire = original.clone();
        wire["policy"]["origins"][0]["ipv4_addresses"] = json!([address]);
        if address == "127.0.0.1" {
            seal_unchecked_policy(&mut wire);
        }
        assert!(parse(&wire).is_err());
    }
    for port in [0, 80, 444, 65535] {
        let mut wire = original.clone();
        wire["policy"]["origins"][0]["port"] = json!(port);
        seal_unchecked_policy(&mut wire);
        assert!(parse(&wire).is_err());
    }
}

#[test]
fn policy_array_sizes_and_nonempty_trusted_inventory_are_bounded() {
    assert!(HostIpv4DenySet::from_trusted_host_inventory([]).is_err());
    assert!(
        HostIpv4DenySet::from_trusted_host_inventory(std::iter::repeat_n(
            Ipv4Addr::LOCALHOST,
            MAX_HOST_IPV4_ADDRESSES + 1
        ))
        .is_err()
    );
    let original = qualification_wire();
    for origins in [
        json!([]),
        json!((0..=MAX_QUALIFICATION_ORIGINS)
            .map(|i| json!({"host":format!("h{i}.example.com"),"port":443,
            "ipv4_addresses":["1.1.1.1"]}))
            .collect::<Vec<_>>()),
    ] {
        let mut wire = original.clone();
        wire["policy"]["origins"] = origins;
        seal_unchecked_policy(&mut wire);
        assert!(parse(&wire).is_err());
    }
    let mut wire = original.clone();
    wire["policy"]["origins"][0]["ipv4_addresses"] = json!([]);
    seal_unchecked_policy(&mut wire);
    assert!(parse(&wire).is_err());
    let mut wire = original.clone();
    wire["policy"]["origins"][0]["ipv4_addresses"] =
        json!(["1.1.1.1", "1.1.1.2", "1.1.1.3", "1.1.1.4", "1.1.1.5"]);
    seal_unchecked_policy(&mut wire);
    assert!(parse(&wire).is_err());
    assert_eq!(
        NetworkProfileV1::from_json(&serde_json::to_vec(&original).unwrap(), &expected(), None)
            .unwrap_err()
            .kind(),
        NetworkErrorKind::MissingHostInventory
    );
}

#[test]
fn no_unchecked_dns_retry_ipv6_nat64_or_changed_host_inventory_is_accepted() {
    let profile = parse(&qualification_wire()).unwrap();
    for (name, port, address) in [
        ("a.example.com", 443, "1.1.1.3"),
        ("b.example.com", 443, "1.1.1.1"),
        ("a.example.com", 80, "1.1.1.1"),
        ("a.example.com.", 443, "1.1.1.1"),
        ("a.example.com", 443, "127.0.0.1"),
        ("a.example.com", 443, "fd00::1"),
        ("a.example.com", 443, "fdaa::3"),
        ("a.example.com", 443, "::ffff:1.1.1.1"),
        ("a.example.com", 443, "64:ff9b::101:101"),
        ("a.example.com", 443, "64:ff9b:1::1"),
        ("a.example.com", 443, "2606:4700:4700::1111"),
    ] {
        assert!(profile
            .check_destination(name, port, address.parse().unwrap(), &host())
            .is_err());
    }
    let changed =
        HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::new(1, 1, 1, 1)]).unwrap();
    assert_eq!(
        profile
            .check_destination("a.example.com", 443, "1.1.1.1".parse().unwrap(), &changed)
            .unwrap_err()
            .kind(),
        NetworkErrorKind::DestinationDenied
    );
    assert_eq!(
        NetworkProfileV1::from_json(
            &serde_json::to_vec(&qualification_wire()).unwrap(),
            &expected(),
            Some(&changed)
        )
        .unwrap_err()
        .kind(),
        NetworkErrorKind::DestinationDenied
    );
}

#[test]
fn conservative_public_ipv4_table_covers_private_special_and_boundary_addresses() {
    for (first, last) in [
        ("0.0.0.0", "0.255.255.255"),
        ("10.0.0.0", "10.255.255.255"),
        ("100.64.0.0", "100.127.255.255"),
        ("127.0.0.0", "127.255.255.255"),
        ("169.254.0.0", "169.254.255.255"),
        ("172.16.0.0", "172.31.255.255"),
        ("192.0.0.0", "192.0.0.255"),
        ("192.0.2.0", "192.0.2.255"),
        ("192.31.196.0", "192.31.196.255"),
        ("192.52.193.0", "192.52.193.255"),
        ("192.88.99.0", "192.88.99.255"),
        ("192.168.0.0", "192.168.255.255"),
        ("192.175.48.0", "192.175.48.255"),
        ("198.18.0.0", "198.19.255.255"),
        ("198.51.100.0", "198.51.100.255"),
        ("203.0.113.0", "203.0.113.255"),
        ("224.0.0.0", "255.255.255.255"),
    ] {
        assert!(
            !is_qualification_public_ipv4(first.parse().unwrap()),
            "{first}"
        );
        assert!(
            !is_qualification_public_ipv4(last.parse().unwrap()),
            "{last}"
        );
    }
    for address in [
        "1.1.1.1",
        "8.8.8.8",
        "100.63.255.255",
        "100.128.0.0",
        "172.15.255.255",
        "172.32.0.0",
        "192.0.1.255",
        "192.0.3.0",
        "198.17.255.255",
        "198.20.0.0",
        "223.255.255.255",
    ] {
        assert!(
            is_qualification_public_ipv4(address.parse().unwrap()),
            "{address}"
        );
    }
}

#[cfg(target_os = "linux")]
mod kernel {
    use super::*;
    use rudder_engine::execution::network::linux::*;
    use std::{
        os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
        process::{Command, Output},
        time::{Duration, Instant},
    };

    fn qualifier(mode: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rudder-network-qualifier"))
            .args(["--qualification-only", mode])
            .env_clear()
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let began = Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if began.elapsed() > Duration::from_secs(20) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned network qualifier did not settle");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.stdout.len() < 64 * 1024 && output.stderr.len() < 4096);
        output
    }
    fn events(output: &Output) -> Vec<Value> {
        std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn unsupported(output: &Output) -> bool {
        if output.status.code() != Some(77) {
            return false;
        }
        let values = events(output);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["status"], "unsupported");
        assert_eq!(values[0]["hostFallback"], false);
        eprintln!(
            "kernel qualification unavailable on this host, not positive runtime proof: {}",
            values[0]
        );
        true
    }
    fn zero(value: &Value) {
        for name in [
            "effective",
            "permitted",
            "inheritable",
            "ambient",
            "bounding",
        ] {
            assert_eq!(value[name], "0000000000000000", "{name}");
        }
        assert_eq!(value["noNewPrivileges"], true);
    }
    fn self_namespaces() -> (std::path::PathBuf, std::path::PathBuf) {
        (
            std::fs::read_link("/proc/self/ns/user").unwrap(),
            std::fs::read_link("/proc/self/ns/net").unwrap(),
        )
    }

    #[test]
    fn namespace_fd_type_validation_rejects_regular_and_wrong_namespace_fds() {
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        assert!(NamespaceFd::inspect(fd, NamespaceKind::Network).is_err());
        let fd: OwnedFd = std::fs::File::open("/proc/self/ns/user").unwrap().into();
        assert!(NamespaceFd::inspect(fd, NamespaceKind::Network).is_err());
        let source = std::fs::File::open("/proc/self/ns/net").unwrap();
        let raw = unsafe { libc::dup(source.as_raw_fd()) };
        assert!(raw >= 0);
        assert_eq!(
            unsafe { libc::fcntl(raw, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let network =
            NamespaceFd::inspect(unsafe { OwnedFd::from_raw_fd(raw) }, NamespaceKind::Network)
                .unwrap();
        assert_eq!(network.identity().kind(), NamespaceKind::Network);
        assert!(
            unsafe { libc::fcntl(network.as_fd().as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC
                != 0
        );
    }

    #[test]
    fn capture_refuses_a_host_or_test_thread_without_changing_its_namespaces() {
        let before = self_namespaces();
        let caps = read_capabilities().unwrap();
        assert!(ChildNetworkBaseline::capture(&parse(&offline_wire()).unwrap()).is_err());
        assert_eq!(self_namespaces(), before);
        assert_eq!(read_capabilities().unwrap(), caps);
    }

    #[test]
    fn fresh_child_refuses_an_unchanged_user_namespace_without_touching_host_network() {
        let before = self_namespaces();
        let output = qualifier("namespace-baseline");
        if !unsupported(&output) {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let values = events(&output);
            assert_eq!(values.len(), 1);
            assert_eq!(values[0]["status"], "verified_no_namespace_change");
            assert_eq!(values[0]["error"], "ForeignNamespace");
        }
        assert_eq!(self_namespaces(), before);
    }

    #[test]
    fn all_five_capability_drop_is_enforced_and_survives_actual_fresh_exec() {
        let before = read_capabilities().unwrap();
        let output = qualifier("drop-and-exec");
        if !unsupported(&output) {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let values = events(&output);
            assert_eq!(values.len(), 2);
            assert_eq!(values[0]["kind"], "capability-drop");
            zero(&values[0]["after"]);
            assert_eq!(values[1]["kind"], "capability-exec-readback");
            zero(&values[1]["after"]);
            assert_eq!(values[1]["liveAttemptAuthorized"], false);
        }
        assert_eq!(read_capabilities().unwrap(), before);
    }

    #[test]
    fn capability_proof_is_not_reusable_in_another_process() {
        let output = qualifier("proof-wrong-process");
        if !unsupported(&output) {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let values = events(&output);
            assert_eq!(values.len(), 1);
            assert_eq!(values[0]["status"], "verified_foreign_process_refused");
        }
    }

    #[test]
    fn capability_proof_is_not_revived_after_another_user_namespace_reset() {
        let before = self_namespaces();
        let output = qualifier("proof-after-user-reset");
        if !unsupported(&output) {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let values = events(&output);
            assert_eq!(values.len(), 1);
            assert_eq!(values[0]["status"], "verified_reset_refused");
        }
        assert_eq!(self_namespaces(), before);
    }

    #[test]
    fn fresh_uid1500_newnet_qualifies_or_explicitly_reports_kernel_refusal_without_fallback() {
        let before = self_namespaces();
        let output = qualifier("namespace-setup");
        if !unsupported(&output) {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let values = events(&output);
            assert_eq!(values.len(), 1);
            let value = &values[0];
            assert_eq!(value["status"], "qualified_kernel_primitives_only");
            assert_eq!(value["uid"], 1500);
            assert_ne!(value["parentUser"], value["childUser"]);
            assert_ne!(value["parentNet"], value["childNet"]);
            assert_eq!(value["loopbackVerified"], true);
            assert_eq!(value["parentWitnessesClosed"], true);
            assert_eq!(value["allowedNamespaceWitnesses"], 2);
            let effective =
                u64::from_str_radix(value["beforeDrop"]["effective"].as_str().unwrap(), 16)
                    .unwrap();
            assert_ne!(effective & (1u64 << 12), 0);
            assert_ne!(value["beforeDrop"]["bounding"], "0000000000000000");
            zero(&value["afterDrop"]);
            assert_eq!(value["egressQualified"], false);
            assert_eq!(value["liveAttemptAuthorized"], false);
            assert_eq!(value["hostFallback"], false);
        }
        assert_eq!(self_namespaces(), before);
    }

    #[test]
    fn new_user_network_setup_closes_parent_witnesses_before_actual_zero_caps_exec() {
        let before = self_namespaces();
        let output = qualifier("namespace-and-exec");
        if !unsupported(&output) {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let values = events(&output);
            assert_eq!(values.len(), 2);
            assert_eq!(values[0]["parentWitnessesClosed"], true);
            assert_eq!(values[0]["allowedNamespaceWitnesses"], 2);
            zero(&values[0]["afterDrop"]);
            assert_eq!(values[1]["kind"], "capability-exec-readback");
            zero(&values[1]["after"]);
        }
        assert_eq!(self_namespaces(), before);
    }

    #[test]
    fn qualifier_requires_explicit_opt_in_before_any_mutation() {
        let output = Command::new(env!("CARGO_BIN_EXE_rudder-network-qualifier"))
            .env_clear()
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(64));
        assert!(output.stdout.is_empty());
    }
}
