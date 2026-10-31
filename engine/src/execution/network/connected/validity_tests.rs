struct HeldGuard(AtomicBool);
impl ConnectValidationGuard for HeldGuard {
    fn check_current(&self) -> Result<()> {
        if self.0.load(Ordering::Acquire) { Ok(()) }
        else { Err(Error::authority_denied()) }
    }
}
fn binding() -> ConnectBinding {
    ConnectBinding::for_profile(&super::super::qualification::test_profile()).unwrap()
}
fn host() -> HostIpv4DenySet {
    HostIpv4DenySet::from_trusted_host_inventory([std::net::Ipv4Addr::LOCALHOST]).unwrap()
}
fn validation() -> ConnectValidation {
    let query = ConnectQuery::begin(&binding()).unwrap(); let fixed = qualifier_deadline(60_000);
    ConnectValidation::new(query, host(), fixed, fixed, fixed, Arc::new(HeldGuard(AtomicBool::new(true)))).unwrap()
}
fn until(mut ready: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !ready() {
        assert!(std::time::Instant::now() < end, "original bound did not expire");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn exact_original_query_identity_cannot_be_replaced_by_matching_binding_data() {
    let binding = binding(); let query = ConnectQuery::begin(&binding).unwrap(); let key = query.identity();
    let other = ConnectQuery::begin(&binding).unwrap(); let fixed = qualifier_deadline(60_000);
    let value = ConnectValidation::new(query, host(), fixed, fixed, fixed, Arc::new(HeldGuard(AtomicBool::new(true)))).unwrap();
    assert!(value.matches(&binding, &key)); assert!(!value.matches(&binding, &other.identity()));
    let mut wrong = binding; wrong.network.owner = "different-owner".into();
    assert!(!value.matches(&wrong, &key));
}

#[test]
fn adoption_cutoff_is_distinct_from_the_original_continuing_window() {
    let value = validation();
    let original = value.query.stale_at;
    until(|| value.query.adopt_by.check(Sample::now().unwrap()).is_err());
    value.check_current().unwrap(); // An already-adopted fast query can continue.
    assert_eq!(value.query.stale_at.wall_ms, original.wall_ms);
    assert_eq!(value.query.stale_at.boot_ns, original.boot_ns);
    assert!(value.check_adoption().is_err()); // Same late value cannot be adopted now.
    assert!(value.check_current().is_err()); // Refusal is absorbing, never renewed.
}

#[test]
fn original_grant_inventory_and_producer_each_refuse_either_expired_clock_at_creation() {
    for expired_component in 0..3 {
        for wall_expired in [false, true] {
            let query = ConnectQuery::begin(&binding()).unwrap(); let now = Sample::now().unwrap();
            let live = qualifier_deadline(60_000);
            let expired = ConnectDeadline::from_original(
                if wall_expired { now.wall_ms - 1 } else { live.wall_ms },
                if wall_expired { live.boot_ns } else { now.boot_ns - 1 }).unwrap();
            let grant = if expired_component == 0 { expired } else { live };
            let inventory = if expired_component == 1 { expired } else { live };
            let producer = if expired_component == 2 { expired } else { live };
            assert!(ConnectValidation::new(query, host(), grant, inventory, producer,
                Arc::new(HeldGuard(AtomicBool::new(true)))).is_err());
        }
    }
}

#[test]
fn clock_regression_and_live_guard_denial_cannot_be_repaired_on_the_same_result() {
    for wall in [false, true] {
        let value = validation(); let actual = Sample::now().unwrap();
        { let mut last = value.last.lock().unwrap();
            if wall { last.wall_ms = actual.wall_ms + 60_000; }
            else { last.boot_ns = actual.boot_ns + 60_000 * NS_PER_MS; } }
        assert!(value.check_bounds().is_err());
        *value.last.lock().unwrap() = actual;
        assert!(value.check_current().is_err());
    }
    let query = ConnectQuery::begin(&binding()).unwrap(); let fixed = qualifier_deadline(60_000);
    let owner = Arc::new(HeldGuard(AtomicBool::new(true)));
    let value = ConnectValidation::new(query, host(), fixed, fixed, fixed, owner.clone()).unwrap();
    owner.0.store(false, Ordering::Release); assert!(value.check_current().is_err());
    owner.0.store(true, Ordering::Release); assert!(value.check_current().is_err());
}
