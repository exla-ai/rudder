use super::*;
use crate::engine_host::{EngineOwnership, NativeAuthority};
use rudder_engine::execution::network::{NetworkBinding, NetworkEpoch};
use std::{collections::{BTreeMap, BTreeSet}, fs::{self, File, OpenOptions},
    net::Ipv4Addr, os::unix::fs::{DirBuilderExt, MetadataExt}, path::{Path, PathBuf},
    sync::{atomic::AtomicU64, mpsc, Condvar}, thread};

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        let root = Path::new("/tmp").canonicalize().unwrap()
            .join(format!("rca-{}", uuid::Uuid::new_v4().simple()));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        Self(root)
    }
}
impl Drop for Temporary { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
struct Fixture {
    owner: Option<EngineOwnership>,
    writer: Journal,
    issuance: Arc<HostConnectIssuance>,
    binding: ConnectBinding,
    repo: PathBuf,
    temporary: Temporary,
}
fn grant(binding: &ConnectBinding, expiry: i64) -> CapabilityGrant {
    CapabilityGrant { principal: host_connect_principal(binding).unwrap(),
        run_id: binding.network.run_id.clone(), run_epoch: binding.network.run_epoch.value(),
        operations: BTreeSet::from([HOST_CONNECT_REQUIRED_OPERATION.into()]),
        recipients: BTreeMap::new(), scopes: BTreeSet::from([host_connect_scope(binding).unwrap()]),
        topics: BTreeSet::new(), expires_at_ms: Some(expiry) }
}
impl Fixture {
    fn prepared() -> Self {
        Self::prepared_with_lifetime(Duration::from_secs(10))
    }
    fn prepared_with_lifetime(lifetime: Duration) -> Self {
        let temporary = Temporary::new();
        let repo = temporary.0.join("repo"); let home = temporary.0.join("home");
        for directory in [&repo, &home] { fs::DirBuilder::new().mode(0o700).create(directory).unwrap(); }
        let mut owner = EngineOwnership::acquire_in_home(&repo, NativeAuthority::Fabric, &home).unwrap();
        let execution_id = uuid::Uuid::new_v4().to_string();
        let (journal, fence) = owner.journal_parts(); let fence = fence.clone();
        let epoch = journal.observe_run_attempt(&fence, "run", &execution_id, 0).unwrap();
        let binding = ConnectBinding { format_version: 1,
            protocol: rudder_engine::execution::network::connected::CONNECT_TRANSPORT_PROTOCOL.into(),
            profile_sha256: "a".repeat(64), network: NetworkBinding {
                project_id: fence.project_id.clone(), owner: fence.owner_id.clone(),
                owner_epoch: NetworkEpoch::new(fence.epoch).unwrap(), run_id: "run".into(),
                run_epoch: NetworkEpoch::new(epoch).unwrap(), execution_id,
                request_id: uuid::Uuid::new_v4().to_string(), operation_id: uuid::Uuid::new_v4().to_string(),
                view_id: uuid::Uuid::new_v4().to_string(), network_id: uuid::Uuid::new_v4().to_string(),
            } };
        // The writer factory is minted from this actual owner, never its saved path.
        let writer_factory = owner.source_journal_factory(&fence).unwrap();
        let writer = writer_factory().unwrap();
        let mut lease = Some(owner.source_lease(&fence).unwrap());
        let mut original = Some(OriginalHostConnectLifetime::begin_issuance(&binding, lifetime).unwrap());
        let selected = grant(&binding, original.as_ref().unwrap().expires_at_ms());
        let issuance = HostConnectIssuance::prepare_retaining(&mut lease, &writer,
            binding.clone(), &selected, &mut original).unwrap();
        assert!(lease.is_none() && original.is_none());
        Self { owner: Some(owner), writer, issuance, binding, repo, temporary }
    }
    fn issued() -> Self { let mut value = Self::prepared(); value.issuance.issue(&mut value.writer).unwrap(); value }
    fn inventory(&self, lifetime: Duration) -> Arc<FixtureInventory> {
        FixtureInventory::new(self.issuance.lease.owner().clone(), self.binding.clone(), lifetime)
    }
    fn authority(&self, inventory: Arc<FixtureInventory>) -> Arc<HostConnectAuthority> {
        let mut issuance = Some(self.issuance.clone()); let mut inventory = Some(inventory::fixture(inventory));
        HostConnectAuthority::new_retaining(&mut issuance, &mut inventory).unwrap()
    }
}

/// Actual held namespace/file custody, but SYNTHETIC address coverage only.
/// This cannot supply FlyVmNoIngressV1 local/public/NAT production provenance.
struct FixtureInventory {
    owner: OwnerFence,
    binding: ConnectBinding,
    namespace: File,
    boot_id: Vec<u8>,
    end: Deadline,
    revision: AtomicU64,
    gate: Mutex<Option<Arc<Gate>>>,
}
struct Gate { entered: mpsc::SyncSender<()>, released: Mutex<bool>, changed: Condvar }
impl Gate {
    fn release(&self) { *self.released.lock().unwrap() = true; self.changed.notify_all(); }
}
struct CallbackWorker {
    gate: Arc<Gate>,
    handle: Option<thread::JoinHandle<Result<NativeValidation>>>,
}
impl CallbackWorker {
    fn is_finished(&self) -> bool { self.handle.as_ref().unwrap().is_finished() }
    fn join(mut self) -> thread::Result<Result<NativeValidation>> {
        self.gate.release();
        self.handle.take().unwrap().join()
    }
}
impl Drop for CallbackWorker {
    fn drop(&mut self) {
        self.gate.release();
        if let Some(handle) = self.handle.take() { let _ = handle.join(); }
    }
}
impl FixtureInventory {
    fn new(owner: OwnerFence, binding: ConnectBinding, lifetime: Duration) -> Arc<Self> {
        let clock = Clock::begin(lifetime).unwrap();
        Arc::new(Self { owner, binding, namespace: File::open("/proc/thread-self/ns/net").unwrap(),
            boot_id: fs::read("/proc/sys/kernel/random/boot_id").unwrap(), end: clock.fixed,
            revision: AtomicU64::new(1), gate: Mutex::new(None) })
    }
    fn custody(&self) -> Result<()> {
        let held = self.namespace.metadata().map_err(|_| HostConnectError::Inventory)?;
        let current = fs::metadata("/proc/thread-self/ns/net").map_err(|_| HostConnectError::Inventory)?;
        if (held.dev(), held.ino()) != (current.dev(), current.ino())
            || fs::read("/proc/sys/kernel/random/boot_id").map_err(|_| HostConnectError::Inventory)? != self.boot_id {
            return Err(HostConnectError::Inventory);
        }
        Ok(())
    }
}
impl inventory::InventoryCustody for FixtureInventory {
    fn snapshot(&self, owner: &OwnerFence, binding: &ConnectBinding, at: Sample) -> Result<inventory::InventorySnapshot> {
        self.custody()?;
        if owner != &self.owner || binding != &self.binding { return Err(HostConnectError::Inventory); }
        self.end.check(at).map_err(|_| HostConnectError::Inventory)?;
        let revision = self.revision.load(Ordering::Acquire);
        let gate = self.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.entered.send(()).map_err(|_| HostConnectError::Inventory)?;
            let mut released = gate.released.lock().unwrap();
            while !*released { released = gate.changed.wait(released).unwrap(); }
        }
        self.custody()?;
        Ok(inventory::InventorySnapshot {
            addresses: HostIpv4DenySet::from_trusted_host_inventory([Ipv4Addr::LOCALHOST]).unwrap(),
            revision, end: self.end,
        })
    }
    fn check_revision(&self, revision: u64) -> Result<()> {
        self.custody()?;
        if revision != self.revision.load(Ordering::Acquire) { return Err(HostConnectError::Inventory); }
        Ok(())
    }
}

#[test]
fn exact_real_owner_grant_and_bounded_readonly_checks_retain_original_clocks() {
    fn implements_engine_authority<T: ConnectAuthority>() {}
    implements_engine_authority::<HostConnectAuthority>();
    let mut fixture = Fixture::issued();
    let inventory = fixture.inventory(Duration::from_secs(5)); let end = inventory.end;
    let authority = fixture.authority(inventory);
    let validated = authority.validate_current(&fixture.binding).unwrap();
    assert!(validated.addresses.contains(Ipv4Addr::LOCALHOST));
    assert_eq!(validated.guard.inventory_end.wall_ms, end.wall_ms);
    assert_eq!(validated.guard.inventory_end.boot_ns, end.boot_ns);
    assert!(validated.guard.producer_end.wall_ms <= fixture.issuance.grant_end.wall_ms);
    validated.guard.check_native().unwrap();
    assert_eq!(fixture.issuance.observe(&fixture.writer).unwrap(), CapabilityIssuanceObservation::Present);
    assert_eq!(fixture.issuance.retire(&mut fixture.writer).unwrap(), CapabilityIssuanceObservation::Revoked);
    assert!(fixture.issuance.capability_is_retired());
    assert!(validated.guard.check_native().is_err());
}

#[test]
fn concurrent_guard_clock_sampling_waits_without_fencing_healthy_authority() {
    let fixture = Fixture::issued();
    let authority = fixture.authority(fixture.inventory(Duration::from_secs(5)));
    let validation = authority.validate_current(&fixture.binding).unwrap();
    let clock = fixture.issuance.clock.lock().unwrap();
    let (entered, ready) = mpsc::channel();
    let (finished, result) = mpsc::channel();
    let worker = thread::spawn(move || {
        entered.send(()).unwrap();
        finished.send(validation.guard.check_native()).unwrap();
    });
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(result.recv_timeout(Duration::from_millis(20)), Err(mpsc::RecvTimeoutError::Timeout)));
    drop(clock);
    result.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
    worker.join().unwrap();
    assert!(!fixture.issuance.denied.load(Ordering::Acquire));
}

#[test]
fn mixed_wrong_scope_nonfinite_and_renewed_grants_leave_actual_inputs_retained() {
    let fixture = Fixture::prepared();
    for kind in ["mixed", "scope", "principal", "source", "unbounded", "renewed"] {
        let mut original = Some(OriginalHostConnectLifetime::begin_issuance(&fixture.binding, Duration::from_secs(5)).unwrap());
        let mut selected = grant(&fixture.binding, original.as_ref().unwrap().expires_at_ms());
        match kind {
            "mixed" => { selected.operations.insert("message:send".into()); }
            "scope" => { selected.scopes = BTreeSet::from(["host-connect:other".into()]); }
            "principal" => selected.principal = "guest".into(),
            "source" => selected.operations = BTreeSet::from(["source:host:authorize".into()]),
            "unbounded" => selected.expires_at_ms = None,
            _ => selected.expires_at_ms = selected.expires_at_ms.map(|at| at + 1),
        }
        let mut lease = Some(fixture.owner.as_ref().unwrap().source_lease(fixture.issuance.lease.owner()).unwrap());
        assert!(HostConnectIssuance::prepare_retaining(&mut lease, &fixture.writer,
            fixture.binding.clone(), &selected, &mut original).is_err(), "{kind}");
        assert!(lease.is_some() && original.is_some());
    }
}

#[test]
fn actual_revocation_and_replaced_execution_fence_existing_guards() {
    for action in ["revoke", "attempt", "owner"] {
        let mut fixture = Fixture::issued();
        let authority = fixture.authority(fixture.inventory(Duration::from_secs(5)));
        let validated = authority.validate_current(&fixture.binding).unwrap();
        match action {
            "revoke" => {
                let prepared = fixture.issuance.prepared.read().unwrap();
                fixture.writer.revoke_capability(fixture.issuance.lease.owner(),
                    prepared.acknowledged_token().unwrap(), wall_ms().unwrap()).unwrap();
            }
            "attempt" => { fixture.writer.observe_run_attempt(fixture.issuance.lease.owner(), "run",
                &uuid::Uuid::new_v4().to_string(), wall_ms().unwrap()).unwrap(); }
            _ => { fixture.owner.as_mut().unwrap().finish(false).unwrap(); }
        }
        assert!(validated.guard.check_native().is_err(), "{action}");
        assert!(authority.validate_current(&fixture.binding).is_err(), "denial stays absorbing");
    }
}

#[test]
fn changed_binding_revision_and_actual_lock_custody_are_absorbing() {
    for action in ["binding", "revision", "lock"] {
        let fixture = Fixture::issued(); let inventory = fixture.inventory(Duration::from_secs(5));
        let authority = fixture.authority(inventory.clone());
        let validated = authority.validate_current(&fixture.binding).unwrap();
        match action {
            "binding" => {
                let mut changed = fixture.binding.clone(); changed.profile_sha256 = "b".repeat(64);
                assert!(authority.validate_current(&changed).is_err());
            }
            "revision" => { inventory.revision.fetch_add(1, Ordering::AcqRel); }
            _ => {
                let lock = fixture.repo.join(".rudder/engine/owner.lock");
                fs::rename(&lock, lock.with_extension("old")).unwrap();
                fs::create_dir(&lock).unwrap();
            }
        }
        assert!(validated.guard.check_native().is_err(), "{action}");
        assert!(authority.validate_current(&fixture.binding).is_err());
    }
}

#[test]
fn late_callback_keeps_actual_owner_charged_until_real_return_and_join() {
    for drop_owner in [false, true] {
        let mut fixture = Fixture::issued(); let inventory = fixture.inventory(Duration::from_secs(5));
        let (entered, entered_rx) = mpsc::sync_channel(1);
        let gate = Arc::new(Gate { entered, released: Mutex::new(false), changed: Condvar::new() });
        *inventory.gate.lock().unwrap() = Some(gate.clone());
        let authority = fixture.authority(inventory); let weak = Arc::downgrade(&authority);
        let binding = fixture.binding.clone();
        let worker = CallbackWorker { gate: gate.clone(),
            handle: Some(thread::spawn(move || authority.validate_current(&binding))) };
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        thread::sleep(Duration::from_millis(PRODUCER_LIMIT_MS as u64 + 25));
        // No local time-out releases the actual callback, issuance, or owner lock.
        assert!(!worker.is_finished()); assert!(weak.upgrade().is_some());
        if !drop_owner {
            gate.release();
            assert!(matches!(worker.join().unwrap(), Err(HostConnectError::Deadline)),
                "a live owner cannot give the late callback a fresh producer cutoff");
            fixture.owner.as_ref().unwrap().assert_current().unwrap();
            assert!(fixture.issuance.denied.load(Ordering::Acquire));
            fixture.issuance.retire(&mut fixture.writer).unwrap();
            continue;
        }
        let lock = OpenOptions::new().read(true).write(true)
            .open(fixture.repo.join(".rudder/engine/owner.lock")).unwrap();
        fixture.issuance.retire(&mut fixture.writer).unwrap();
        assert!(fixture.issuance.capability_is_retired());
        assert!(!worker.is_finished(), "capability retirement is not callback settlement");
        drop(fixture.owner.take()); drop(fixture.issuance);
        assert!(fs2::FileExt::try_lock_exclusive(&lock).is_err());
        gate.release();
        assert!(worker.join().unwrap().is_err());
        assert!(weak.upgrade().is_none());
        fs2::FileExt::try_lock_exclusive(&lock).unwrap(); fs2::FileExt::unlock(&lock).unwrap();
        // Keep the fixture directory until after the actual retained writer closes.
        drop(fixture.writer); drop(fixture.temporary);
    }
}

#[test]
fn local_fencing_and_busy_retirement_do_not_claim_capability_or_callback_settlement() {
    let mut fixture = Fixture::issued();
    let authority = fixture.authority(fixture.inventory(Duration::from_secs(5)));
    let validated = authority.validate_current(&fixture.binding).unwrap();
    fixture.issuance.fence_admission();
    assert!(validated.guard.check_native().is_err());
    assert_eq!(fixture.issuance.observe(&fixture.writer).unwrap(), CapabilityIssuanceObservation::Present);
    assert!(!fixture.issuance.capability_is_retired());
    let reader = fixture.issuance.prepared.read().unwrap();
    assert_eq!(fixture.issuance.retire(&mut fixture.writer), Err(HostConnectError::Unsettled));
    assert!(!fixture.issuance.capability_is_retired());
    drop(reader);
    assert_eq!(fixture.issuance.retire(&mut fixture.writer).unwrap(), CapabilityIssuanceObservation::Revoked);
    assert!(fixture.issuance.capability_is_retired());
}

#[test]
fn committed_error_and_panic_keep_actual_issuance_for_observe_and_retryable_retirement() {
    for panic in [false, true] {
        let mut fixture = Fixture::prepared();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fixture.issuance.issue_inner(&mut fixture.writer, || {
                if panic { panic!("synthetic native ACK loss after actual commit"); }
                Err(HostConnectError::Unsettled)
            })
        }));
        assert!(!matches!(result, Ok(Ok(()))));
        assert_eq!(fixture.issuance.observe(&fixture.writer).unwrap(), CapabilityIssuanceObservation::Present);
        assert!(!fixture.issuance.acknowledged.load(Ordering::Acquire));
        assert!(fixture.issuance.issue(&mut fixture.writer).is_err());
        let mut issuance = Some(fixture.issuance.clone());
        let mut inventory = Some(inventory::fixture(fixture.inventory(Duration::from_secs(5))));
        assert!(HostConnectAuthority::new_retaining(&mut issuance, &mut inventory).is_err());
        assert!(issuance.is_some() && inventory.is_some());
        assert_eq!(fixture.issuance.retire(&mut fixture.writer).unwrap(), CapabilityIssuanceObservation::Revoked);
        assert_eq!(fixture.issuance.retire(&mut fixture.writer).unwrap(), CapabilityIssuanceObservation::Revoked);
        assert!(fixture.issuance.capability_is_retired());
    }
}

#[test]
fn expired_original_grant_or_inventory_cannot_gain_a_callback_lifetime() {
    let mut fixture = Fixture::issued();
    let inventory = fixture.inventory(Duration::from_millis(1));
    let original_end = inventory.end;
    thread::sleep(Duration::from_millis(5));
    let authority = fixture.authority(inventory.clone());
    assert!(authority.validate_current(&fixture.binding).is_err());
    assert_eq!(original_end.wall_ms, inventory.end.wall_ms);
    // Cleanup is independent of the failed/expired authorization clock.
    assert_eq!(fixture.issuance.retire(&mut fixture.writer).unwrap(), CapabilityIssuanceObservation::Revoked);

    let mut fixture = Fixture::prepared_with_lifetime(Duration::from_secs(1));
    fixture.issuance.issue(&mut fixture.writer).unwrap();
    let original_end = fixture.issuance.grant_end;
    thread::sleep(Duration::from_millis(1_025));
    let mut issuance = Some(fixture.issuance.clone());
    let mut inventory = Some(inventory::fixture(fixture.inventory(Duration::from_secs(5))));
    assert!(HostConnectAuthority::new_retaining(&mut issuance, &mut inventory).is_err());
    assert!(issuance.is_some() && inventory.is_some());
    assert_eq!(fixture.issuance.grant_end.wall_ms, original_end.wall_ms);
    assert_eq!(fixture.issuance.observe(&fixture.writer).unwrap(), CapabilityIssuanceObservation::Present);
    assert_eq!(fixture.issuance.retire(&mut fixture.writer).unwrap(), CapabilityIssuanceObservation::Revoked);
}

#[test]
fn clock_regression_is_absorbing_and_original_issuance_time_cannot_shift_on_prepare_retry() {
    let fixture = Fixture::prepared();
    let other = Fixture::prepared();
    let mut original = Some(OriginalHostConnectLifetime::begin_issuance(&fixture.binding, Duration::from_secs(5)).unwrap());
    let issued = original.as_ref().unwrap().issued_at_ms;
    let selected = grant(&fixture.binding, original.as_ref().unwrap().expires_at_ms());
    let mut lease = Some(fixture.owner.as_ref().unwrap().source_lease(fixture.issuance.lease.owner()).unwrap());
    assert!(HostConnectIssuance::prepare_retaining(&mut lease, &other.writer,
        fixture.binding.clone(), &selected, &mut original).is_err());
    assert!(lease.is_some() && original.is_some());
    assert_eq!(original.as_ref().unwrap().issued_at_ms, issued);
    let retried = HostConnectIssuance::prepare_retaining(&mut lease, &fixture.writer,
        fixture.binding.clone(), &selected, &mut original).unwrap();
    assert_eq!(retried.issued_at_ms, issued);
    assert_eq!(retried.grant_end.wall_ms, selected.expires_at_ms.unwrap());
    // Exercise both absolute expiry axes without a wall-only/arrival-time conversion.
    let now = retried.clock.lock().unwrap().last;
    assert!(Deadline { wall_ms: now.wall_ms, boot_ns: now.boot_ns + NS_PER_MS }.check(now).is_err());
    assert!(Deadline { wall_ms: now.wall_ms + 1, boot_ns: now.boot_ns }.check(now).is_err());
    let mut clock = Clock::begin(Duration::from_secs(1)).unwrap();
    clock.last.boot_ns = u128::MAX;
    assert!(clock.sample().is_err());
    clock.last.boot_ns = 0;
    assert!(clock.sample().is_err(), "a corrected sample cannot revive a failed clock");
}
