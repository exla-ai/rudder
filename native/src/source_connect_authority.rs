#![cfg(target_os = "linux")]
//! Retained host CONNECT authority. No source/profile activation or scheduler.
//! FlyVmNoIngressV1 still lacks an authenticated complete inventory producer and
//! real boot/NET custody. There is deliberately no production inventory constructor.
//! The application must retain issuance/inventory plus its bounded worker charge
//! through actual callback/proxy joins. Fence admission, retire the capability
//! while its original Engine owner/run is current, then join off-App before
//! releasing Engine ownership. Capability retirement alone proves no stopped worker.
//! Current checks are observations, not atomic synchronization with later revocation.
use crate::engine_host::EngineSourceLease;
use rudder_engine::{
    execution::network::{connected::{ConnectAuthority, ConnectBinding, ConnectDeadline,
        ConnectError, ConnectQuery, ConnectResult, ConnectValidation, ConnectValidationGuard},
        HostIpv4DenySet},
    fleet_lease::{HostClock, SuspendAwareClock},
    journal::{host_connect_principal, host_connect_scope, CapabilityGrant,
        CapabilityIssuanceObservation, HostConnectAuthorization, Journal, OwnerFence,
        PreparedCapabilityIssuance, HOST_CONNECT_REQUIRED_OPERATION},
};
use std::{sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, RwLock, TryLockError},
    time::{Duration, SystemTime, UNIX_EPOCH}};

const QUERY_BUSY_TIMEOUT: Duration = Duration::from_millis(100);
const PRODUCER_LIMIT_MS: i64 = 250;
const NS_PER_MS: u128 = 1_000_000;

/// Static diagnostics only; never expose a token, Journal error or inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostConnectError { Fenced, Binding, Journal, Inventory, Clock, Deadline, Unsettled }
impl std::fmt::Display for HostConnectError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "host CONNECT authorization failed ({:?})", self)
    }
}
impl std::error::Error for HostConnectError {}
type Result<T> = std::result::Result<T, HostConnectError>;

#[derive(Clone, Copy)]
struct Sample { wall_ms: i64, boot_ns: u128 }
#[derive(Clone, Copy)]
struct Deadline { wall_ms: i64, boot_ns: u128 }
impl Deadline {
    fn producer(start: Sample, fixed: Self) -> Result<Self> {
        Ok(Self {
            wall_ms: start.wall_ms.checked_add(PRODUCER_LIMIT_MS)
                .ok_or(HostConnectError::Clock)?.min(fixed.wall_ms),
            boot_ns: start.boot_ns.checked_add(PRODUCER_LIMIT_MS as u128 * NS_PER_MS)
                .ok_or(HostConnectError::Clock)?.min(fixed.boot_ns),
        })
    }
    fn check(self, at: Sample) -> Result<()> {
        if at.wall_ms >= self.wall_ms || at.boot_ns >= self.boot_ns {
            return Err(HostConnectError::Deadline);
        }
        Ok(())
    }
    fn engine(self) -> ConnectResult<ConnectDeadline> {
        ConnectDeadline::from_original(self.wall_ms, self.boot_ns)
    }
}
struct Clock { source: HostClock, last: Sample, fixed: Deadline, failed: bool }
impl Clock {
    fn begin(lifetime: Duration) -> Result<Self> {
        let ms = i64::try_from(lifetime.as_millis()).map_err(|_| HostConnectError::Clock)?;
        if !(1..=86_400_000).contains(&ms) { return Err(HostConnectError::Deadline); }
        let source = HostClock::new().map_err(|_| HostConnectError::Clock)?;
        // BOOTTIME precedes wall capture, so the original local lifetime is conservative.
        let boot_ns = source.read().map_err(|_| HostConnectError::Clock)?.nanoseconds();
        let wall_ms = wall_ms()?;
        let fixed = Deadline { wall_ms: wall_ms.checked_add(ms).ok_or(HostConnectError::Clock)?,
            boot_ns: boot_ns.checked_add(ms as u128 * NS_PER_MS).ok_or(HostConnectError::Clock)? };
        Ok(Self { source, last: Sample { wall_ms, boot_ns }, fixed, failed: false })
    }
    fn sample(&mut self) -> Result<Sample> {
        if self.failed { return Err(HostConnectError::Clock); }
        let result = (|| {
            let wall_ms = wall_ms()?;
            let boot_ns = self.source.read().map_err(|_| HostConnectError::Clock)?.nanoseconds();
            if wall_ms < self.last.wall_ms || boot_ns < self.last.boot_ns {
                return Err(HostConnectError::Clock);
            }
            let at = Sample { wall_ms, boot_ns };
            self.fixed.check(at)?;
            self.last = at;
            Ok(at)
        })();
        if result.is_err() { self.failed = true; }
        result
    }
}
fn wall_ms() -> Result<i64> {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)
        .map_err(|_| HostConnectError::Clock)?.as_millis()).map_err(|_| HostConnectError::Clock)
}

/// Begin before preparing/issuing the selected grant. No saved-time constructor,
/// Clone or Serde: this same owner crosses preparation, issue and uncertainty.
pub(crate) struct OriginalHostConnectLifetime {
    binding: ConnectBinding,
    issued_at_ms: i64,
    clock: Clock,
}
impl OriginalHostConnectLifetime {
    pub(crate) fn begin_issuance(binding: &ConnectBinding, lifetime: Duration) -> Result<Self> {
        binding.validate().map_err(|_| HostConnectError::Binding)?;
        let clock = Clock::begin(lifetime)?;
        Ok(Self { binding: binding.clone(), issued_at_ms: clock.last.wall_ms, clock })
    }
    pub(crate) fn expires_at_ms(&self) -> i64 { self.clock.fixed.wall_ms }
}

/// The actual issuance owner, held BEFORE invoking issue. No token injection or
/// token accessor exists. Err/panic retains the prepared capability and lease;
/// observing Present DATA cannot restore the native original acknowledgment.
#[must_use = "retain through exact capability retirement and actual callback/worker settlement"]
pub(crate) struct HostConnectIssuance {
    lease: EngineSourceLease,
    binding: ConnectBinding,
    prepared: RwLock<PreparedCapabilityIssuance>,
    clock: Mutex<Clock>,
    issued_at_ms: i64,
    grant_end: Deadline,
    attempted: AtomicBool,
    acknowledged: AtomicBool,
    denied: AtomicBool,
    retired: AtomicBool,
}
struct DenyOnUnwind<'a>(&'a HostConnectIssuance);
impl Drop for DenyOnUnwind<'_> {
    fn drop(&mut self) { if std::thread::panicking() { self.0.deny(); } }
}
impl HostConnectIssuance {
    /// OFF-APP. Every fallible operation precedes transferring the input slots.
    /// The writer must belong to this actual held Engine owner and Journal.
    pub(crate) fn prepare_retaining(lease: &mut Option<EngineSourceLease>, journal: &Journal,
        binding: ConnectBinding, selected: &CapabilityGrant,
        original: &mut Option<OriginalHostConnectLifetime>) -> Result<Arc<Self>> {
        binding.validate().map_err(|_| HostConnectError::Binding)?;
        let held = lease.as_ref().ok_or(HostConnectError::Fenced)?;
        let lifetime = original.as_mut().ok_or(HostConnectError::Clock)?;
        let owner = held.owner();
        if lifetime.binding != binding || owner.project_id != binding.network.project_id
            || owner.owner_id != binding.network.owner
            || owner.epoch != binding.network.owner_epoch.value()
            || selected.principal != host_connect_principal(&binding).map_err(|_| HostConnectError::Binding)?
            || selected.run_id != binding.network.run_id
            || selected.run_epoch != binding.network.run_epoch.value()
            || selected.operations.len() != 1 || !selected.operations.contains(HOST_CONNECT_REQUIRED_OPERATION)
            || selected.scopes.len() != 1
            || !selected.scopes.contains(&host_connect_scope(&binding).map_err(|_| HostConnectError::Binding)?)
            || !selected.recipients.is_empty() || !selected.topics.is_empty()
            || selected.expires_at_ms != Some(lifetime.expires_at_ms()) {
            return Err(HostConnectError::Binding);
        }
        let issued_at_ms = lifetime.issued_at_ms;
        lifetime.clock.sample()?;
        held.check_journal(journal).map_err(|_| HostConnectError::Journal)?;
        if journal.current_run_attempt(owner, &binding.network.run_id, binding.network.run_epoch.value())
            .map_err(|_| HostConnectError::Journal)? != binding.network.execution_id {
            return Err(HostConnectError::Binding);
        }
        let prepared = journal.prepare_capability_issuance(owner, selected, issued_at_ms)
            .map_err(|_| HostConnectError::Journal)?;
        lifetime.clock.sample()?;
        held.check_current().map_err(|_| HostConnectError::Fenced)?;
        let clock = original.take().unwrap().clock;
        Ok(Arc::new(Self { lease: lease.take().unwrap(), binding, issued_at_ms,
            grant_end: clock.fixed, clock: Mutex::new(clock), prepared: RwLock::new(prepared),
            attempted: AtomicBool::new(false), acknowledged: AtomicBool::new(false),
            denied: AtomicBool::new(false), retired: AtomicBool::new(false) }))
    }
    fn deny(&self) { self.denied.store(true, Ordering::Release); }
    /// App-safe local denial only. This neither revokes the durable capability
    /// nor proves proxy/callback settlement. Retire before releasing Engine ownership.
    pub(crate) fn fence_admission(&self) { self.deny(); }
    fn sample(&self) -> Result<Sample> {
        if self.denied.load(Ordering::Acquire) { return Err(HostConnectError::Fenced); }
        // Relay and monitor checks share this short clock-only section. Ordinary
        // contention is not clock uncertainty and must not retire healthy grants.
        let mut clock = self.clock.lock().map_err(|_| HostConnectError::Clock)?;
        if self.denied.load(Ordering::Acquire) { return Err(HostConnectError::Fenced); }
        clock.sample() // Recheck the original expiry after acquiring the lock.
    }
    fn check_at(&self, deadline: Deadline) -> Result<Sample> {
        if !self.acknowledged.load(Ordering::Acquire) { return Err(HostConnectError::Fenced); }
        self.lease.check_current().map_err(|_| HostConnectError::Fenced)?;
        let at = self.sample()?;
        deadline.check(at)?;
        Ok(at)
    }
    /// OFF-APP; invoke only after the caller has retained this owner/charge.
    pub(crate) fn issue(&self, journal: &mut Journal) -> Result<()> {
        self.issue_inner(journal, || Ok(()))
    }
    fn issue_inner(&self, journal: &mut Journal, after_write: impl FnOnce() -> Result<()>) -> Result<()> {
        let _unwind = DenyOnUnwind(self);
        let result = (|| {
            if self.attempted.swap(true, Ordering::AcqRel) { return Err(HostConnectError::Fenced); }
            self.lease.check_journal(journal).map_err(|_| HostConnectError::Journal)?;
            let at = self.sample()?;
            let mut prepared = self.prepared.try_write().map_err(|_| HostConnectError::Unsettled)?;
            journal.issue_prepared_capability(&mut prepared, at.wall_ms).map_err(|_| HostConnectError::Journal)?;
            after_write()?; // Private committed-but-unacknowledged/panic test seam.
            self.sample()?;
            self.lease.check_current().map_err(|_| HostConnectError::Fenced)?;
            self.acknowledged.store(true, Ordering::Release);
            Ok(())
        })();
        if result.is_err() { self.deny(); }
        result
    }
    /// Current DATA only. Expired/denied owners may reconcile, never reactivate.
    pub(crate) fn observe(&self, journal: &Journal) -> Result<CapabilityIssuanceObservation> {
        let result = (|| {
            self.lease.check_journal(journal).map_err(|_| HostConnectError::Journal)?;
            let prepared = match self.prepared.try_read() {
                Ok(value) => value, Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => return Err(HostConnectError::Unsettled),
            };
            journal.observe_prepared_capability(&prepared).map_err(|_| HostConnectError::Journal)
        })();
        if result.is_err() { self.deny(); }
        result
    }
    /// OFF-APP retryable cleanup, including after expiry or an issue panic. A
    /// busy reader/failed write retains the SAME owner. This proves only exact
    /// capability retirement, never proxy/helper/writer or callback settlement.
    pub(crate) fn retire(&self, journal: &mut Journal) -> Result<CapabilityIssuanceObservation> {
        self.deny();
        self.acknowledged.store(false, Ordering::Release);
        self.lease.check_journal(journal).map_err(|_| HostConnectError::Journal)?;
        let mut prepared = match self.prepared.try_write() {
            Ok(value) => value, Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(HostConnectError::Unsettled),
        };
        let result = journal.retire_prepared_capability(&mut prepared, wall_ms()?)
            .map_err(|_| HostConnectError::Journal)?;
        self.retired.store(prepared.is_retired(), Ordering::Release);
        self.lease.check_current().map_err(|_| HostConnectError::Fenced)?;
        Ok(result)
    }
    pub(crate) fn capability_is_retired(&self) -> bool { self.retired.load(Ordering::Acquire) }
    fn authorize(&self, binding: &ConnectBinding, deadline: Deadline) -> Result<()> {
        if binding != &self.binding { return Err(HostConnectError::Binding); }
        self.check_at(deadline)?;
        let journal = self.lease.open_journal(QUERY_BUSY_TIMEOUT).map_err(|_| HostConnectError::Journal)?;
        let at = self.check_at(deadline)?;
        // Borrow the original token from actual issuance custody. Concurrent
        // read-only checks can coexist; retirement fences before trying write.
        let prepared = self.prepared.try_read().map_err(|_| HostConnectError::Unsettled)?;
        let token = prepared.acknowledged_token().map_err(|_| HostConnectError::Fenced)?;
        let authorization = journal.authorize_host_connect(token, binding, self.grant_end.wall_ms, at.wall_ms);
        self.check_at(deadline)?;
        self.exact_snapshot(authorization.map_err(|_| HostConnectError::Journal)?, at.wall_ms)
    }
    fn exact_snapshot(&self, authorization: HostConnectAuthorization, query_ms: i64) -> Result<()> {
        let identity = authorization.identity;
        if identity.project_id != self.binding.network.project_id
            || identity.principal != host_connect_principal(&self.binding).map_err(|_| HostConnectError::Binding)?
            || identity.run_id != self.binding.network.run_id
            || identity.run_epoch != self.binding.network.run_epoch.value()
            || identity.owner_epoch != self.lease.owner().epoch
            || authorization.validated_at_ms != query_ms || authorization.issued_at_ms != self.issued_at_ms
            || authorization.expires_at_ms != self.grant_end.wall_ms {
            return Err(HostConnectError::Journal);
        }
        Ok(())
    }
}

/// Sealed seam. Only an authenticated complete inventory producer INSIDE this
/// module may eventually mint custody. JSON, getifaddrs, host labels and caller
/// callbacks cannot mint it. FlyVmNoIngressV1 production custody is missing.
mod inventory {
    use super::*;
    pub(super) trait InventoryCustody: Send + Sync {
        // Capture revision and original authenticated response bounds before
        // callback work; invalidate before any uncertain/address-change window.
        fn snapshot(&self, owner: &OwnerFence, binding: &ConnectBinding, at: Sample) -> Result<InventorySnapshot>;
        fn check_revision(&self, revision: u64) -> Result<()>;
    }
    pub(crate) struct TrustedHostInventoryLease { custody: Arc<dyn InventoryCustody> }
    pub(super) struct InventorySnapshot {
        pub(super) addresses: HostIpv4DenySet,
        pub(super) revision: u64,
        /// Original authenticated response admission bounds, never callback-arrival time.
        pub(super) end: Deadline,
    }
    impl TrustedHostInventoryLease {
        pub(super) fn snapshot(&self, owner: &OwnerFence, binding: &ConnectBinding, at: Sample) -> Result<InventorySnapshot> {
            self.custody.snapshot(owner, binding, at)
        }
        pub(super) fn check_revision(&self, revision: u64) -> Result<()> { self.custody.check_revision(revision) }
    }
    impl InventorySnapshot {
        pub(super) fn check(&self, at: Sample) -> Result<()> {
            if self.revision == 0 { return Err(HostConnectError::Inventory); }
            self.end.check(at).map_err(|_| HostConnectError::Inventory)
        }
    }
    #[cfg(test)]
    pub(super) fn fixture(custody: Arc<dyn InventoryCustody>) -> TrustedHostInventoryLease {
        TrustedHostInventoryLease { custody }
    }
}
pub(crate) use inventory::TrustedHostInventoryLease;

pub(crate) struct HostConnectAuthority {
    issuance: Arc<HostConnectIssuance>,
    inventory: TrustedHostInventoryLease,
}
struct HostConnectGuard {
    authority: Arc<HostConnectAuthority>,
    revision: u64,
    inventory_end: Deadline,
    producer_end: Deadline,
}
struct NativeValidation { guard: Arc<HostConnectGuard>, addresses: HostIpv4DenySet }
impl HostConnectAuthority {
    /// OFF-APP. Failed construction leaves both retained owners in their slots.
    /// No raw token, serialized grant or saved expiry can construct this value.
    pub(crate) fn new_retaining(issuance: &mut Option<Arc<HostConnectIssuance>>,
        inventory: &mut Option<TrustedHostInventoryLease>) -> Result<Arc<Self>> {
        let held = issuance.as_ref().ok_or(HostConnectError::Fenced)?;
        let result = (|| {
            if inventory.is_none() { return Err(HostConnectError::Inventory); }
            held.check_at(held.grant_end)?;
            Ok(())
        })();
        if result.is_err() { held.deny(); }
        result?;
        Ok(Arc::new(Self { issuance: issuance.take().unwrap(), inventory: inventory.take().unwrap() }))
    }
    fn validate_current(self: &Arc<Self>, binding: &ConnectBinding) -> Result<NativeValidation> {
        let _unwind = DenyOnUnwind(&self.issuance);
        let result = (|| {
            // Capture BEFORE filesystem, SQLite or inventory work. Preserve this
            // same strict 250ms producer bound for admission AND continuing use.
            let start = self.issuance.sample()?;
            let producer_end = Deadline::producer(start, self.issuance.grant_end)?;
            self.issuance.authorize(binding, producer_end)?;
            let snapshot = self.inventory.snapshot(self.issuance.lease.owner(), binding,
                self.issuance.check_at(producer_end)?)?;
            snapshot.check(self.issuance.check_at(producer_end)?)?;
            self.inventory.check_revision(snapshot.revision)?;
            snapshot.check(self.issuance.check_at(producer_end)?)?;
            Ok(NativeValidation { guard: Arc::new(HostConnectGuard { authority: self.clone(),
                revision: snapshot.revision, inventory_end: snapshot.end, producer_end }), addresses: snapshot.addresses })
        })();
        if result.is_err() { self.issuance.deny(); }
        result
    }
}
impl HostConnectGuard {
    fn check_native(&self) -> Result<()> {
        let _unwind = DenyOnUnwind(&self.authority.issuance);
        let result = (|| {
            let held = &self.authority.issuance;
            self.inventory_end.check(held.check_at(self.producer_end)?)
                .map_err(|_| HostConnectError::Inventory)?;
            held.authorize(&held.binding, self.producer_end)?;
            self.inventory_end.check(held.check_at(self.producer_end)?)
                .map_err(|_| HostConnectError::Inventory)?;
            self.authority.inventory.check_revision(self.revision)?;
            self.inventory_end.check(held.check_at(self.producer_end)?)
                .map_err(|_| HostConnectError::Inventory)
        })();
        if result.is_err() { self.authority.issuance.deny(); }
        result
    }
}
impl ConnectValidationGuard for HostConnectGuard {
    fn check_current(&self) -> ConnectResult<()> { self.check_native().map_err(|_| ConnectError::authority_denied()) }
}
impl ConnectAuthority for HostConnectAuthority {
    fn validate(self: Arc<Self>, query: ConnectQuery) -> ConnectResult<ConnectValidation> {
        let result = (|| {
            let native = self.validate_current(query.binding()).map_err(|_| ConnectError::authority_denied())?;
            // Move the EXACT engine query, including its private identity. Copy
            // only original absolute bounds; neither callback return nor guard
            // adoption grants any new time. Engine independently retains 200ms
            // admission/500ms staleness; this producer stays stricter at 250ms.
            ConnectValidation::new(query, native.addresses, self.issuance.grant_end.engine()?,
                native.guard.inventory_end.engine()?, native.guard.producer_end.engine()?, native.guard)
        })();
        if result.is_err() { self.issuance.deny(); }
        result
    }
}

#[cfg(test)]
#[path = "source_connect_authority_tests.rs"]
mod tests;
