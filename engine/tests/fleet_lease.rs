use rudder_engine::fleet_lease::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

const AUTHORITY: &str = "00000000-0000-0000-0000-000000000001";
const PROJECT: &str = "00000000-0000-0000-0000-000000000002";
const LEASE: &str = "00000000-0000-0000-0000-000000000003";
const OTHER: &str = "00000000-0000-0000-0000-000000000004";
const MS: u128 = 1_000_000;

#[derive(Clone)]
struct Clock(Arc<Mutex<Result<ClockReading, ClockFault>>>);
impl Clock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Ok(ClockReading::new(1, 0)))))
    }
    fn nanos(&self, nanos: u128) {
        *self.0.lock().unwrap() = Ok(ClockReading::new(1, nanos));
    }
    fn ms(&self, ms: u128) {
        self.nanos(ms * MS);
    }
    fn fail(&self, error: ClockFault) {
        *self.0.lock().unwrap() = Err(error);
    }
    fn reset(&self, nanos: u128) {
        *self.0.lock().unwrap() = Ok(ClockReading::new(2, nanos));
    }
}
impl SuspendAwareClock for Clock {
    fn read(&self) -> Result<ClockReading, ClockFault> {
        *self.0.lock().unwrap()
    }
}
fn safety() -> LeaseSafety {
    // Same vector as frozen tests/project-authority.test.mjs.
    LeaseSafety {
        margin_ms: 100.0,
        engine_rate_error: 0.0,
        database_rate_error: 0.0,
        max_positive_clock_step_ms: 50.0,
    }
}
fn wire(process: &EngineProcessIdentity, request: &str, serial: &str) -> Value {
    json!({
        "formatVersion":1,"authorityId":AUTHORITY,"accountId":"account",
        "projectId":PROJECT,"engineInstanceId":process.engine_instance_id(),
        "machineBootId":process.machine_boot_id(),"ownerEpoch":"2","leaseId":LEASE,
        "renewalSerial":serial,"requestId":request,"issuedAt":"untrusted-wall-clock",
        "leaseUntil":"untrusted-wall-clock","durationMs":1000
    })
}
fn grant(value: &Value) -> OwnerGrant {
    OwnerGrant::from_json(&serde_json::to_vec(value).unwrap()).unwrap()
}
struct Fixture {
    process: EngineProcessIdentity,
    authenticated: AuthenticatedAuthority,
    clock: Clock,
    fence: LeaseFence<Clock>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_safety(safety())
    }
    fn with_safety(safety: LeaseSafety) -> Self {
        let process = EngineProcessIdentity::new("boot").unwrap();
        let identity = process
            .owner_identity(AUTHORITY, "account", PROJECT, "2", LEASE)
            .unwrap();
        let clock = Clock::new();
        let fence =
            LeaseFence::with_clock_for_qualification(identity, safety, clock.clone()).unwrap();
        Self {
            process,
            authenticated: AuthenticatedAuthority::new(AUTHORITY, "account").unwrap(),
            clock,
            fence,
        }
    }
    fn observe(&mut self, request: &str, serial: &str) -> bool {
        self.fence.observe(
            &self.authenticated,
            &grant(&wire(&self.process, request, serial)),
        )
    }
    fn active() -> Self {
        let mut f = Self::new();
        f.clock.ms(100);
        f.fence.begin("first").unwrap();
        f.clock.ms(101);
        assert!(f.observe("first", "0"));
        f
    }
}

#[test]
fn camel_case_wire_is_strict_and_counters_never_become_json_numbers() {
    let process = EngineProcessIdentity::new("boot").unwrap();
    let original = wire(&process, "request", "9223372036854775807");
    let receipt = grant(&original);
    assert_eq!(serde_json::to_value(&receipt).unwrap(), original);
    assert_eq!(receipt.renewal_serial().value(), i64::MAX as u64);
    assert_eq!(receipt.duration_ms(), 1000);
    let mut invalid = Vec::new();
    for (key, replacement) in [
        ("extra", json!(true)),
        ("formatVersion", json!(2)),
        ("format_version", json!(1)),
        ("ownerEpoch", json!("0")),
        ("renewalSerial", json!(1)),
        ("ownerEpoch", json!(2)),
        ("durationMs", json!(0)),
        ("durationMs", json!(15001)),
        ("durationMs", json!(1.5)),
        ("durationMs", json!(-1)),
        ("authorityId", json!("00000000-0000-0000-0000-00000000000A")),
        ("projectId", json!("project")),
        ("accountId", json!("")),
        ("machineBootId", json!("bad\0boot")),
        ("issuedAt", json!("x".repeat(181))),
        ("leaseUntil", json!("")),
        ("requestId", json!("x".repeat(181))),
    ] {
        let mut value = original.clone();
        value[key] = replacement;
        invalid.push(value);
    }
    for field in [
        "authorityId",
        "requestId",
        "durationMs",
        "issuedAt",
        "leaseUntil",
    ] {
        let mut value = original.clone();
        value.as_object_mut().unwrap().remove(field);
        invalid.push(value);
    }
    for value in invalid {
        assert_eq!(
            OwnerGrant::from_json(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
            LeaseError::InvalidGrant
        );
    }
    let duplicate = serde_json::to_string(&original).unwrap().replacen(
        "\"formatVersion\":1",
        "\"formatVersion\":1,\"formatVersion\":1",
        1,
    );
    assert!(OwnerGrant::from_json(duplicate.as_bytes()).is_err());
    assert!(OwnerGrant::from_json(&vec![b' '; 4097]).is_err());
}

#[test]
fn canonical_counter_and_signed_bigint_overflow_reject_without_truncation() {
    for value in ["0", "1", "9223372036854775807"] {
        let counter = Counter::parse(value).unwrap();
        assert_eq!(serde_json::to_value(counter).unwrap(), json!(value));
    }
    for value in [
        "",
        "00",
        "01",
        "+1",
        "-1",
        " 1",
        "1 ",
        "1.0",
        "1e2",
        "١",
        "9223372036854775808",
        "18446744073709551615",
        "100000000000000000000",
    ] {
        assert_eq!(Counter::parse(value), Err(LeaseError::InvalidCounter));
        let mut receipt = wire(&EngineProcessIdentity::new("boot").unwrap(), "request", "0");
        receipt["renewalSerial"] = json!(value);
        assert!(OwnerGrant::from_json(&serde_json::to_vec(&receipt).unwrap()).is_err());
    }
}

#[test]
fn first_send_survives_same_nonce_retries_and_expiry_is_an_exact_boundary() {
    let mut f = Fixture::new();
    assert!(!f.fence.current());
    f.clock.ms(100);
    f.fence.begin("request").unwrap();
    f.clock.ms(300);
    f.fence.begin("request").unwrap();
    assert_eq!(f.fence.begin("other"), Err(LeaseError::Busy));
    f.clock.ms(500);
    assert!(f.observe("request", "0"));
    f.clock.nanos(950 * MS - 2);
    assert!(f.fence.current());
    f.clock.nanos(950 * MS - 1);
    assert!(!f.fence.current());
    assert_eq!(f.fence.fenced_reason(), Some(FenceReason::Expired));
    f.clock.ms(951);
    assert_eq!(f.fence.begin("new"), Err(LeaseError::Fenced));
    assert!(!f.observe("request", "1"));
}

#[test]
fn parsed_receipts_and_future_server_wall_clocks_do_not_create_or_extend_permission() {
    let mut f = Fixture::new();
    let mut receipt = wire(&f.process, "request", "0");
    receipt["issuedAt"] = json!("2099-12-31T23:59:59.999Z");
    receipt["leaseUntil"] = json!("9999-12-31T23:59:59.999Z");
    let receipt = grant(&receipt);
    assert!(!f.fence.observe(&f.authenticated, &receipt));
    assert!(!f.fence.current());
    f.clock.ms(100);
    f.fence.begin("request").unwrap();
    f.clock.ms(101);
    assert!(f.fence.observe(&f.authenticated, &receipt));
    f.clock.ms(950);
    assert!(!f.fence.current());
}

#[test]
fn numeric_vectors_match_frozen_ts_and_native_rounding_is_conservative() {
    // Verified with frozen MonotonicLeaseFence: each TS fence is live at
    // native_ns/1e6 and expired at its unrounded safe_duration. Native keeps
    // floor(TS_duration*1e6)-1ns; no platform floating-point deadline is saved.
    for (duration, margin, engine, database, step, native_ns) in [
        (1000, 100.0, 0.0, 0.0, 50.0, 849_999_999u128),
        (15000, 20.0, 0.001, 0.002, 100.0, 14_835_389_220),
        (1, 0.0, 0.0, 0.0, 0.0, 999_999),
        (15000, 0.0, 0.5, 0.5, 0.0, 4_999_999_999),
        (1000, 0.25, 0.01, 0.03, 0.125, 960_794_901),
    ] {
        let mut f = Fixture::with_safety(LeaseSafety {
            margin_ms: margin,
            engine_rate_error: engine,
            database_rate_error: database,
            max_positive_clock_step_ms: step,
        });
        f.fence.begin("vector").unwrap();
        let mut receipt = wire(&f.process, "vector", "0");
        receipt["durationMs"] = json!(duration);
        assert!(f.fence.observe(&f.authenticated, &grant(&receipt)));
        f.clock.nanos(native_ns - 1);
        assert!(f.fence.current());
        f.clock.nanos(native_ns);
        assert!(!f.fence.current());
    }
}

#[test]
fn exact_authenticated_identity_and_nonce_are_required_without_extending_old_grants() {
    for (field, mismatch) in [
        ("authorityId", OTHER),
        ("accountId", "other"),
        ("projectId", OTHER),
        ("engineInstanceId", OTHER),
        ("machineBootId", "old-boot"),
        ("ownerEpoch", "3"),
        ("leaseId", OTHER),
        ("requestId", "foreign"),
    ] {
        let mut f = Fixture::active();
        f.clock.ms(200);
        f.fence.begin("renew").unwrap();
        let mut receipt = wire(&f.process, "renew", "1");
        receipt[field] = json!(mismatch);
        f.clock.ms(300);
        assert!(
            !f.fence.observe(&f.authenticated, &grant(&receipt)),
            "{field}"
        );
        f.clock.ms(949);
        assert!(f.fence.current());
        f.clock.ms(950);
        assert!(!f.fence.current(), "mismatch extended {field}");
    }
    for context in [
        AuthenticatedAuthority::new(OTHER, "account").unwrap(),
        AuthenticatedAuthority::new(AUTHORITY, "other").unwrap(),
    ] {
        let mut f = Fixture::active();
        f.clock.ms(200);
        f.fence.begin("renew").unwrap();
        f.clock.ms(300);
        assert!(!f
            .fence
            .observe(&context, &grant(&wire(&f.process, "renew", "1"))));
        f.clock.ms(950);
        assert!(!f.fence.current());
    }
}

#[test]
fn rejected_responses_keep_the_pending_first_send_and_newer_serial_renews_once() {
    let mut f = Fixture::active();
    f.clock.ms(200);
    f.fence.begin("renew").unwrap();
    f.clock.ms(300);
    assert!(!f.observe("renew", "0"));
    f.clock.ms(400);
    f.fence.begin("renew").unwrap();
    f.clock.ms(500);
    assert!(f.observe("renew", "1"));
    assert!(
        !f.observe("renew", "2"),
        "no pending request cannot accept a receipt"
    );
    f.clock.ms(1049);
    assert!(f.fence.current());
    f.clock.ms(1050);
    assert!(!f.fence.current());
}

#[test]
fn delayed_initial_grant_and_unpolled_late_renewal_cannot_revive_an_expired_fence() {
    let mut initial = Fixture::new();
    initial.clock.ms(100);
    initial.fence.begin("initial").unwrap();
    initial.clock.ms(950);
    assert!(!initial.observe("initial", "0"));
    assert_eq!(initial.fence.begin("retry"), Err(LeaseError::Fenced));
    let mut f = Fixture::active();
    f.clock.ms(800);
    f.fence.begin("renew").unwrap();
    f.clock.ms(951);
    assert!(!f.observe("renew", "1"));
    assert_eq!(f.fence.fenced_reason(), Some(FenceReason::Expired));
}

#[test]
fn durable_retry_receipt_does_not_replay_or_extend_and_max_serial_never_wraps() {
    let mut f = Fixture::new();
    f.clock.ms(100);
    f.fence.begin("once").unwrap();
    f.clock.ms(101);
    assert!(f.observe("once", "9223372036854775807"));
    f.clock.ms(200);
    f.fence.begin("once").unwrap();
    assert!(!f.observe("once", "9223372036854775807"));
    assert!(!f.observe("once", "0"));
    f.clock.ms(950);
    assert!(!f.fence.current());
}

#[test]
fn clock_regression_from_every_operation_is_permanent_and_new_requests_do_not_reset_it() {
    for operation in ["begin", "observe", "current"] {
        let mut f = Fixture::active();
        f.clock.ms(500);
        assert!(f.fence.current());
        f.clock.ms(499);
        match operation {
            "begin" => assert_eq!(f.fence.begin("new"), Err(LeaseError::Fenced)),
            "observe" => assert!(!f.observe("unknown", "1")),
            _ => assert!(!f.fence.current()),
        }
        assert_eq!(f.fence.fenced_reason(), Some(FenceReason::ClockRegression));
        f.clock.ms(501);
        assert!(!f.fence.current());
        assert_eq!(f.fence.begin("repair"), Err(LeaseError::Fenced));
    }
}

#[test]
fn clock_reset_origin_is_detected_even_when_the_new_value_moves_forward() {
    let mut f = Fixture::active();
    f.clock.reset(300 * MS);
    assert!(!f.fence.current());
    assert_eq!(f.fence.fenced_reason(), Some(FenceReason::ClockReset));
    f.clock.ms(400);
    assert!(!f.fence.current());
}

#[test]
fn nonfinite_negative_overflow_and_read_errors_permanently_fence_every_operation() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, f64::MAX] {
        assert_eq!(
            ClockReading::from_milliseconds(1, value),
            Err(ClockFault::InvalidReading)
        );
    }
    for error in [
        ClockFault::Unavailable,
        ClockFault::InvalidReading,
        ClockFault::ContinuityLost,
    ] {
        for operation in ["begin", "observe", "current"] {
            let mut f = Fixture::active();
            f.clock.fail(error);
            match operation {
                "begin" => assert_eq!(f.fence.begin("new"), Err(LeaseError::Fenced)),
                "observe" => assert!(!f.observe("old", "1")),
                _ => assert!(!f.fence.current()),
            }
            assert_eq!(f.fence.fenced_reason(), Some(FenceReason::ClockFault));
            f.clock.ms(102);
            assert!(!f.fence.current());
        }
    }
}

#[test]
fn restart_resume_boot_and_authority_uncertainty_are_irreversible() {
    for reason in [
        FenceReason::Explicit,
        FenceReason::RestartUncertain,
        FenceReason::ResumeUncertain,
        FenceReason::MachineBootChanged,
        FenceReason::AuthorityUncertain,
    ] {
        let mut f = Fixture::active();
        f.fence.fence(reason);
        f.fence.fence(FenceReason::Explicit);
        assert_eq!(f.fence.fenced_reason(), Some(reason));
        assert!(!f.fence.current());
        assert_eq!(f.fence.begin("fresh"), Err(LeaseError::Fenced));
        assert!(!f.observe("first", "1"));
    }
}

#[test]
fn process_restart_and_reconstruction_cannot_restore_a_saved_receipt() {
    let mut old = Fixture::active();
    let saved = grant(&wire(&old.process, "first", "0"));
    let duplicate = old
        .process
        .owner_identity(AUTHORITY, "account", PROJECT, "2", LEASE)
        .unwrap();
    old.fence.fence(FenceReason::RestartUncertain);
    drop(old.fence);
    assert!(matches!(
        LeaseFence::with_clock_for_qualification(duplicate, safety(), old.clock),
        Err(LeaseError::BindingAlreadyUsed)
    ));
    let process = EngineProcessIdentity::new("boot").unwrap();
    assert_ne!(
        process.engine_instance_id(),
        old.process.engine_instance_id()
    );
    let identity = process
        .owner_identity(AUTHORITY, "account", PROJECT, "2", LEASE)
        .unwrap();
    let mut restarted =
        LeaseFence::with_clock_for_qualification(identity, safety(), Clock::new()).unwrap();
    assert!(!restarted.observe(&old.authenticated, &saved));
    restarted.begin("first").unwrap();
    assert!(!restarted.observe(&old.authenticated, &saved));
    assert!(!restarted.current());
}

#[test]
fn invalid_safety_nonpositive_duration_and_deadline_overflow_fail_closed() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
        for field in 0..4 {
            let mut bounds = safety();
            match field {
                0 => bounds.margin_ms = value,
                1 => bounds.engine_rate_error = value,
                2 => bounds.database_rate_error = value,
                _ => bounds.max_positive_clock_step_ms = value,
            }
            let process = EngineProcessIdentity::new("boot").unwrap();
            let identity = process
                .owner_identity(AUTHORITY, "account", PROJECT, "2", LEASE)
                .unwrap();
            assert!(matches!(
                LeaseFence::with_clock_for_qualification(identity, bounds, Clock::new()),
                Err(LeaseError::InvalidSafety)
            ));
        }
    }
    for (engine, database) in [(1.0, 0.0), (0.0, 1.0), (2.0, 0.0)] {
        let process = EngineProcessIdentity::new("boot").unwrap();
        let identity = process
            .owner_identity(AUTHORITY, "account", PROJECT, "2", LEASE)
            .unwrap();
        let bounds = LeaseSafety {
            engine_rate_error: engine,
            database_rate_error: database,
            ..safety()
        };
        assert!(matches!(
            LeaseFence::with_clock_for_qualification(identity, bounds, Clock::new()),
            Err(LeaseError::InvalidSafety)
        ));
    }
    for step in [1000.0, 2000.0, f64::MAX] {
        let mut f = Fixture::with_safety(LeaseSafety {
            max_positive_clock_step_ms: step,
            ..safety()
        });
        f.fence.begin("unsafe").unwrap();
        assert!(!f.observe("unsafe", "0"));
        assert_eq!(f.fence.fenced_reason(), Some(FenceReason::UnsafeDuration));
    }
    let mut f = Fixture::new();
    f.clock.nanos(u128::MAX - 50);
    f.fence.begin("overflow").unwrap();
    assert!(!f.observe("overflow", "0"));
    assert_eq!(f.fence.fenced_reason(), Some(FenceReason::DeadlineOverflow));
}

#[test]
fn one_engine_identity_serves_independent_projects_and_project_expiry_is_isolated() {
    let mut first = Fixture::active();
    let second_identity = first
        .process
        .owner_identity(AUTHORITY, "account", OTHER, "2", LEASE)
        .unwrap();
    let mut second =
        LeaseFence::with_clock_for_qualification(second_identity, safety(), first.clock.clone())
            .unwrap();
    first.clock.ms(800);
    second.begin("second").unwrap();
    let mut receipt = wire(&first.process, "second", "0");
    receipt["projectId"] = json!(OTHER);
    first.clock.ms(801);
    assert!(second.observe(&first.authenticated, &grant(&receipt)));
    first.clock.ms(950);
    assert!(!first.fence.current());
    assert!(second.current());
    first.clock.ms(1650);
    assert!(!second.current());
}

#[test]
fn used_binding_registry_is_bounded_and_capacity_does_not_allow_reuse() {
    let process = EngineProcessIdentity::new("boot").unwrap();
    for n in 0..4096 {
        let project = Uuid::from_u128(n + 1).to_string();
        let identity = process
            .owner_identity(AUTHORITY, "account", &project, "2", LEASE)
            .unwrap();
        drop(LeaseFence::with_clock_for_qualification(identity, safety(), Clock::new()).unwrap());
    }
    let project = Uuid::from_u128(5000).to_string();
    let identity = process
        .owner_identity(AUTHORITY, "account", &project, "2", LEASE)
        .unwrap();
    assert!(matches!(
        LeaseFence::with_clock_for_qualification(identity, safety(), Clock::new()),
        Err(LeaseError::BindingLimit)
    ));
    let project = Uuid::from_u128(1).to_string();
    let duplicate = process
        .owner_identity(AUTHORITY, "account", &project, "2", LEASE)
        .unwrap();
    assert!(matches!(
        LeaseFence::with_clock_for_qualification(duplicate, safety(), Clock::new()),
        Err(LeaseError::BindingAlreadyUsed)
    ));
}

#[test]
fn actual_host_clock_and_same_process_fence_advance_without_wall_clock_input() {
    let clock = HostClock::new().unwrap();
    let first = clock.read().unwrap();
    std::thread::sleep(Duration::from_millis(2));
    let second = clock.read().unwrap();
    assert!(second.nanoseconds() > first.nanoseconds());
    let process = EngineProcessIdentity::new("trusted-test-boot").unwrap();
    let identity = process
        .owner_identity(AUTHORITY, "account", PROJECT, "2", LEASE)
        .unwrap();
    let mut fence = LeaseFence::new(identity, safety()).unwrap();
    assert!(!fence.current());
    fence.begin("live").unwrap();
    assert!(fence.observe(
        &AuthenticatedAuthority::new(AUTHORITY, "account").unwrap(),
        &grant(&wire(&process, "live", "0"))
    ));
    assert!(fence.current());
    fence.fence(FenceReason::ResumeUncertain);
    assert!(!fence.current());
}
