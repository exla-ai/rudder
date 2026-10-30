//! Inactive fleet lease evidence for a future authenticated native bridge.
//!
//! A parsed receipt is historical data, not permission. Only a process-local
//! fence, sampled immediately before an effect, can report a current lease.
//! Nothing in this module starts a scheduler, performs I/O to an authority, or
//! qualifies provider failover or hypervisor suspension. An unqualified resume,
//! restore, boot change or authority discontinuity must call `fence` first.
//!
//! OS clock references:
//! - Linux CLOCK_BOOTTIME includes OS suspend:
//!   <https://man7.org/linux/man-pages/man3/clock_gettime.3.html>
//! - Apple's continuous Mach clock advances through sleep, unlike absolute time:
//!   <https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/mach/mach_time.h>
//!   <https://developer.apple.com/documentation/kernel/1646199-mach_continuous_time>
//!
//! These OS statements do not establish clock behavior during a Fly host pause.
//! Parsing or matching configured strings does not authenticate a response;
//! this module supplies no cryptographic, TLS or provider HA qualification.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const MAX_GRANT_BYTES: usize = 4096;
const MAX_BINDINGS_PER_PROCESS: usize = 4096;
const NS_PER_MS: u128 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseError {
    InvalidIdentity,
    InvalidCounter,
    InvalidGrant,
    InvalidRequest,
    InvalidSafety,
    BindingAlreadyUsed,
    BindingLimit,
    Busy,
    ClockUnavailable,
    Fenced,
}
impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never echo a receipt, credential, helper response, or caller value.
        write!(f, "fleet lease: {self:?}")
    }
}
impl std::error::Error for LeaseError {}

/// Canonical JSON decimal string in PostgreSQL's nonnegative signed BIGINT range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Counter(u64);
impl Counter {
    pub fn parse(value: &str) -> Result<Self, LeaseError> {
        if value.is_empty()
            || value.len() > 19
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(LeaseError::InvalidCounter);
        }
        let parsed = value
            .parse::<u64>()
            .map_err(|_| LeaseError::InvalidCounter)?;
        if parsed > i64::MAX as u64 {
            return Err(LeaseError::InvalidCounter);
        }
        Ok(Self(parsed))
    }
    pub fn value(self) -> u64 {
        self.0
    }
}
impl Serialize for Counter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}
impl<'de> Deserialize<'de> for Counter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(<D::Error as serde::de::Error>::custom)
    }
}

fn text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 180 && !value.contains('\0')
}
fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(n, b)| {
            if matches!(n, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GrantWire {
    format_version: u8,
    authority_id: String,
    account_id: String,
    project_id: String,
    engine_instance_id: String,
    machine_boot_id: String,
    owner_epoch: Counter,
    lease_id: String,
    renewal_serial: Counter,
    request_id: String,
    issued_at: String,
    lease_until: String,
    duration_ms: u32,
}

/// Frozen TS `OwnerGrant` wire format. Wall-clock fields are opaque audit data.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct OwnerGrant(GrantWire);
impl OwnerGrant {
    pub fn from_json(bytes: &[u8]) -> Result<Self, LeaseError> {
        if bytes.len() > MAX_GRANT_BYTES {
            return Err(LeaseError::InvalidGrant);
        }
        serde_json::from_slice(bytes).map_err(|_| LeaseError::InvalidGrant)
    }
    pub fn renewal_serial(&self) -> Counter {
        self.0.renewal_serial
    }
    pub fn duration_ms(&self) -> u32 {
        self.0.duration_ms
    }
}
impl<'de> Deserialize<'de> for OwnerGrant {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = GrantWire::deserialize(deserializer)
            .map_err(|_| <D::Error as serde::de::Error>::custom("invalid owner grant"))?;
        if wire.format_version != 1
            || !uuid(&wire.authority_id)
            || !uuid(&wire.project_id)
            || !uuid(&wire.engine_instance_id)
            || !uuid(&wire.lease_id)
            || !text(&wire.account_id)
            || !text(&wire.machine_boot_id)
            || wire.owner_epoch.0 == 0
            || !text(&wire.request_id)
            || !text(&wire.issued_at)
            || !text(&wire.lease_until)
            || !(1..=15_000).contains(&wire.duration_ms)
        {
            return Err(<D::Error as serde::de::Error>::custom(
                "invalid owner grant",
            ));
        }
        Ok(Self(wire))
    }
}

/// Construct only from the trusted helper's authenticated connection context.
/// This constructor validates identifiers; it does not authenticate a response.
/// Deliberately has no Deserialize implementation.
pub struct AuthenticatedAuthority {
    authority_id: String,
    account_id: String,
}
impl AuthenticatedAuthority {
    pub fn new(authority_id: &str, account_id: &str) -> Result<Self, LeaseError> {
        if !uuid(authority_id) || !text(account_id) {
            return Err(LeaseError::InvalidIdentity);
        }
        Ok(Self {
            authority_id: authority_id.into(),
            account_id: account_id.into(),
        })
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct LeaseKey {
    authority_id: String,
    account_id: String,
    project_id: String,
    owner_epoch: Counter,
    lease_id: String,
}
struct ProcessState {
    pid: u32,
    engine_instance_id: String,
    machine_boot_id: String,
    used: Mutex<HashSet<LeaseKey>>,
}
/// Fresh once per engine process, then cloned across its projects. The future
/// bridge must bind EngineOwnership.instance_id to this chosen engine identity.
/// Never remint it per request, lease or retry. Never restore
/// this identity from disk. A new instance gets a fresh engine UUID even during
/// the same machine boot; inherited process state fails after fork.
#[derive(Clone)]
pub struct EngineProcessIdentity(Arc<ProcessState>);
impl EngineProcessIdentity {
    pub fn new(trusted_machine_boot_id: &str) -> Result<Self, LeaseError> {
        if !text(trusted_machine_boot_id) {
            return Err(LeaseError::InvalidIdentity);
        }
        Ok(Self(Arc::new(ProcessState {
            pid: std::process::id(),
            engine_instance_id: Uuid::new_v4().to_string(),
            machine_boot_id: trusted_machine_boot_id.into(),
            used: Mutex::new(HashSet::new()),
        })))
    }
    pub fn engine_instance_id(&self) -> &str {
        &self.0.engine_instance_id
    }
    pub fn machine_boot_id(&self) -> &str {
        &self.0.machine_boot_id
    }
    pub fn owner_identity(
        &self,
        authority_id: &str,
        account_id: &str,
        project_id: &str,
        owner_epoch: &str,
        lease_id: &str,
    ) -> Result<OwnerIdentity, LeaseError> {
        let owner_epoch = Counter::parse(owner_epoch)?;
        if self.0.pid != std::process::id()
            || !uuid(authority_id)
            || !text(account_id)
            || !uuid(project_id)
            || owner_epoch.0 == 0
            || !uuid(lease_id)
        {
            return Err(LeaseError::InvalidIdentity);
        }
        Ok(OwnerIdentity {
            process: self.clone(),
            key: LeaseKey {
                authority_id: authority_id.into(),
                account_id: account_id.into(),
                project_id: project_id.into(),
                owner_epoch,
                lease_id: lease_id.into(),
            },
        })
    }
}
/// Trusted exact ownership binding. It is neither cloneable nor deserializable.
pub struct OwnerIdentity {
    process: EngineProcessIdentity,
    key: LeaseKey,
}
impl OwnerIdentity {
    fn claim_once(&self) -> Result<(), LeaseError> {
        let mut used = self.process.0.used.lock().map_err(|_| LeaseError::Fenced)?;
        if used.contains(&self.key) {
            return Err(LeaseError::BindingAlreadyUsed);
        }
        if used.len() >= MAX_BINDINGS_PER_PROCESS {
            return Err(LeaseError::BindingLimit);
        }
        used.insert(self.key.clone());
        Ok(())
    }
    fn matches(&self, authenticated: &AuthenticatedAuthority, grant: &GrantWire) -> bool {
        authenticated.authority_id == self.key.authority_id
            && authenticated.account_id == self.key.account_id
            && grant.authority_id == self.key.authority_id
            && grant.account_id == self.key.account_id
            && grant.project_id == self.key.project_id
            && grant.owner_epoch == self.key.owner_epoch
            && grant.lease_id == self.key.lease_id
            && grant.engine_instance_id == self.process.0.engine_instance_id
            && grant.machine_boot_id == self.process.0.machine_boot_id
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LeaseSafety {
    pub margin_ms: f64,
    pub engine_rate_error: f64,
    pub database_rate_error: f64,
    pub max_positive_clock_step_ms: f64,
}
impl LeaseSafety {
    fn validate(self) -> Result<(), LeaseError> {
        let values = [
            self.margin_ms,
            self.engine_rate_error,
            self.database_rate_error,
            self.max_positive_clock_step_ms,
        ];
        if values.iter().any(|v| !v.is_finite() || *v < 0.0)
            || self.engine_rate_error >= 1.0
            || self.database_rate_error >= 1.0
        {
            return Err(LeaseError::InvalidSafety);
        }
        Ok(())
    }
    fn duration_ns(self, duration_ms: u32) -> Option<u128> {
        // Same operation order as frozen TS MonotonicLeaseFence. Round down to
        // nanoseconds and reserve one more ns for floating-point/clock rounding.
        let duration = (duration_ms as f64 - self.max_positive_clock_step_ms)
            * (1.0 - self.engine_rate_error)
            / (1.0 + self.database_rate_error)
            - self.margin_ms;
        let nanos = (duration * NS_PER_MS as f64).floor();
        if !nanos.is_finite() || nanos < 2.0 {
            return None;
        }
        (nanos as u128).checked_sub(1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockFault {
    Unavailable,
    InvalidReading,
    ContinuityLost,
}

/// An opaque clock sample, not a lease deadline and not serializable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockReading {
    origin: u128,
    nanos: u128,
}
impl ClockReading {
    pub fn new(origin: u128, nanos: u128) -> Self {
        Self { origin, nanos }
    }
    pub fn nanoseconds(self) -> u128 {
        self.nanos
    }
    /// Adapter/qualification seam. Nonfinite, negative and overflowing source
    /// readings cannot enter the integer monotonic state.
    pub fn from_milliseconds(origin: u128, milliseconds: f64) -> Result<Self, ClockFault> {
        let nanos = milliseconds * NS_PER_MS as f64;
        if !milliseconds.is_finite()
            || milliseconds < 0.0
            || !nanos.is_finite()
            || nanos >= u128::MAX as f64
        {
            return Err(ClockFault::InvalidReading);
        }
        Ok(Self::new(origin, nanos.floor() as u128))
    }
}
/// A trusted, suspend-aware clock source. The production constructor always
/// selects HostClock. Injected implementations are for protocol qualification,
/// and do not qualify a clock or provider for live fleet effects.
pub trait SuspendAwareClock {
    fn read(&self) -> Result<ClockReading, ClockFault>;
}

pub struct HostClock {
    origin: u128,
    #[cfg(target_os = "macos")]
    timebase: libc::mach_timebase_info,
}
impl HostClock {
    pub fn new() -> Result<Self, ClockFault> {
        #[cfg(target_os = "macos")]
        let timebase = {
            let mut value = libc::mach_timebase_info { numer: 0, denom: 0 };
            if unsafe { libc::mach_timebase_info(&mut value) } != 0
                || value.numer == 0
                || value.denom == 0
            {
                return Err(ClockFault::Unavailable);
            }
            value
        };
        let clock = Self {
            origin: Uuid::new_v4().as_u128(),
            #[cfg(target_os = "macos")]
            timebase,
        };
        clock.read()?;
        Ok(clock)
    }
}
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_continuous_time() -> u64;
}
impl SuspendAwareClock for HostClock {
    fn read(&self) -> Result<ClockReading, ClockFault> {
        #[cfg(target_os = "linux")]
        {
            let mut value = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
                return Err(ClockFault::Unavailable);
            }
            if value.tv_sec < 0 || !(0..1_000_000_000).contains(&value.tv_nsec) {
                return Err(ClockFault::InvalidReading);
            }
            let nanos = (value.tv_sec as u128)
                .checked_mul(1_000_000_000)
                .and_then(|n| n.checked_add(value.tv_nsec as u128))
                .ok_or(ClockFault::InvalidReading)?;
            Ok(ClockReading::new(self.origin, nanos))
        }
        #[cfg(target_os = "macos")]
        {
            let ticks = unsafe { mach_continuous_time() };
            let nanos = (ticks as u128)
                .checked_mul(self.timebase.numer as u128)
                .ok_or(ClockFault::InvalidReading)?
                / self.timebase.denom as u128;
            Ok(ClockReading::new(self.origin, nanos))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(ClockFault::Unavailable)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FenceReason {
    Explicit,
    Expired,
    ClockFault,
    ClockRegression,
    ClockReset,
    ProcessChanged,
    RestartUncertain,
    ResumeUncertain,
    MachineBootChanged,
    AuthorityUncertain,
    UnsafeDuration,
    DeadlineOverflow,
}
struct Pending {
    request_id: String,
    first_send: u128,
}

/// One process and one ownership grant. No Clone/Serialize/Deserialize, no wall
/// clock, no restore API, and no operation accepting a caller's current time.
/// A true `current()` applies only to that immediate observation; it is never a
/// transferable permission token. The trusted host must fence resume uncertainty.
pub struct LeaseFence<C = HostClock> {
    identity: OwnerIdentity,
    safety: LeaseSafety,
    clock: C,
    last: ClockReading,
    deadline: Option<u128>,
    serial: Option<Counter>,
    pending: Option<Pending>,
    fenced: Option<FenceReason>,
}
impl LeaseFence<HostClock> {
    pub fn new(identity: OwnerIdentity, safety: LeaseSafety) -> Result<Self, LeaseError> {
        Self::with_clock_for_qualification(
            identity,
            safety,
            HostClock::new().map_err(|_| LeaseError::ClockUnavailable)?,
        )
    }
}
impl<C: SuspendAwareClock> LeaseFence<C> {
    /// Deterministic protocol seam; supplying a clock does not establish its
    /// suitability for production, suspension, failover or a live authority.
    pub fn with_clock_for_qualification(
        identity: OwnerIdentity,
        safety: LeaseSafety,
        clock: C,
    ) -> Result<Self, LeaseError> {
        safety.validate()?;
        if identity.process.0.pid != std::process::id() {
            return Err(LeaseError::Fenced);
        }
        let last = clock.read().map_err(|_| LeaseError::ClockUnavailable)?;
        identity.claim_once()?;
        Ok(Self {
            identity,
            safety,
            clock,
            last,
            deadline: None,
            serial: None,
            pending: None,
            fenced: None,
        })
    }
    fn sample(&mut self) -> Result<u128, LeaseError> {
        if self.fenced.is_some() {
            return Err(LeaseError::Fenced);
        }
        if self.identity.process.0.pid != std::process::id() {
            self.fence(FenceReason::ProcessChanged);
            return Err(LeaseError::Fenced);
        }
        let reading = match self.clock.read() {
            Ok(reading) => reading,
            Err(_) => {
                self.fence(FenceReason::ClockFault);
                return Err(LeaseError::Fenced);
            }
        };
        let fault = if reading.origin != self.last.origin {
            Some(FenceReason::ClockReset)
        } else if reading.nanos < self.last.nanos {
            Some(FenceReason::ClockRegression)
        } else {
            None
        };
        if let Some(reason) = fault {
            self.fence(reason);
            return Err(LeaseError::Fenced);
        }
        self.last = reading;
        if self
            .deadline
            .is_some_and(|deadline| reading.nanos >= deadline)
        {
            self.fence(FenceReason::Expired);
            return Err(LeaseError::Fenced);
        }
        Ok(reading.nanos)
    }
    /// Call immediately before first send. Same-nonce retries retain that first
    /// timestamp; another outstanding nonce is rejected without resetting time.
    pub fn begin(&mut self, request_id: &str) -> Result<(), LeaseError> {
        let now = self.sample()?;
        if !text(request_id) {
            return Err(LeaseError::InvalidRequest);
        }
        if let Some(pending) = &self.pending {
            return if pending.request_id == request_id {
                Ok(())
            } else {
                Err(LeaseError::Busy)
            };
        }
        self.pending = Some(Pending {
            request_id: request_id.into(),
            first_send: now,
        });
        Ok(())
    }
    /// Only the authenticated helper may provide the context. Mismatched or
    /// stale responses leave the previous deadline and pending first-send intact.
    pub fn observe(&mut self, authenticated: &AuthenticatedAuthority, grant: &OwnerGrant) -> bool {
        let now = match self.sample() {
            Ok(now) => now,
            Err(_) => return false,
        };
        let Some(pending) = &self.pending else {
            return false;
        };
        if pending.request_id != grant.0.request_id
            || !self.identity.matches(authenticated, &grant.0)
            || self
                .serial
                .is_some_and(|serial| grant.0.renewal_serial <= serial)
        {
            return false;
        }
        let Some(duration) = self.safety.duration_ns(grant.0.duration_ms) else {
            self.fence(FenceReason::UnsafeDuration);
            return false;
        };
        let Some(deadline) = pending.first_send.checked_add(duration) else {
            self.fence(FenceReason::DeadlineOverflow);
            return false;
        };
        if deadline <= now {
            self.fence(FenceReason::Expired);
            return false;
        }
        self.pending = None;
        self.deadline = Some(deadline);
        self.serial = Some(grant.0.renewal_serial);
        true
    }
    pub fn current(&mut self) -> bool {
        self.sample().is_ok() && self.serial.is_some()
    }
    pub fn fenced_reason(&self) -> Option<FenceReason> {
        self.fenced
    }
    pub fn fence(&mut self, reason: FenceReason) {
        self.fenced.get_or_insert(reason);
        self.pending = None;
        self.deadline = None;
    }
}
