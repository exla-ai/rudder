//! Actual retained validation, separate from its non-authorizing time DATA.
use super::{ConnectBinding, ConnectError as Error, ConnectErrorKind as Kind,
    ConnectResult as Result, HostIpv4DenySet};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};

const NS_PER_MS: u128 = 1_000_000;
pub(crate) const ADOPTION_LIMIT_MS: i64 = 200;
pub(crate) const MAX_STALENESS_MS: i64 = 500;

#[derive(Clone, Copy)]
struct Sample { wall_ms:i64, boot_ns:u128 }
impl Sample {
    fn now() -> Result<Self> {
        let mut boot = libc::timespec { tv_sec:0, tv_nsec:0 };
        let mut wall = libc::timespec { tv_sec:0, tv_nsec:0 };
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut boot) } != 0
            || unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut wall) } != 0
            || boot.tv_sec < 0 || wall.tv_sec < 0
            || !(0..1_000_000_000).contains(&boot.tv_nsec)
            || !(0..1_000_000_000).contains(&wall.tv_nsec) {
            return Err(Error::new(Kind::AuthorityDenied));
        }
        Ok(Self { wall_ms:i64::try_from(wall.tv_sec).ok()
                .and_then(|s|s.checked_mul(1000)).and_then(|s|s.checked_add(wall.tv_nsec as i64 / 1_000_000))
                .ok_or_else(||Error::new(Kind::AuthorityDenied))?,
            boot_ns:(boot.tv_sec as u128) * 1_000_000_000 + boot.tv_nsec as u128 })
    }
}

/// Original absolute wall/BOOTTIME bounds, DATA only. The trusted adapter must
/// copy the grant/inventory producer's original bounds, never now + a TTL.
/// No Serde, wall-only conversion or engine constructor can grant authority.
#[derive(Clone, Copy)]
pub struct ConnectDeadline { wall_ms:i64, boot_ns:u128 }
impl ConnectDeadline {
    pub fn from_original(wall_ms:i64, boot_ns:u128) -> Result<Self> {
        if wall_ms <= 0 || boot_ns == 0 { return Err(Error::new(Kind::InvalidConfiguration)); }
        Ok(Self { wall_ms, boot_ns })
    }
    fn after(start: Sample, ms:i64) -> Result<Self> {
        Self::from_original(start.wall_ms.checked_add(ms).ok_or_else(||Error::new(Kind::Deadline))?,
            start.boot_ns.checked_add(ms as u128 * NS_PER_MS).ok_or_else(||Error::new(Kind::Deadline))?)
    }
    fn check(self, sample: Sample) -> Result<()> {
        if sample.wall_ms >= self.wall_ms || sample.boot_ns >= self.boot_ns {
            Err(Error::new(Kind::AuthorityDenied))
        } else { Ok(()) }
    }
}

/// Created only by Runtime BEFORE calling the trusted authority. The private
/// identity prevents returning another query's validation for matching DATA.
/// Adoption and continuing max-staleness are distinct original deadlines.
pub struct ConnectQuery {
    binding:ConnectBinding,
    identity:Arc<()>,
    started:Sample,
    adopt_by:ConnectDeadline,
    stale_at:ConnectDeadline,
}
impl ConnectQuery {
    pub(crate) fn begin(binding: &ConnectBinding) -> Result<Self> {
        let started = Sample::now()?;
        Ok(Self { binding:binding.clone(), identity:Arc::new(()), started,
            adopt_by:ConnectDeadline::after(started, ADOPTION_LIMIT_MS)?,
            stale_at:ConnectDeadline::after(started, MAX_STALENESS_MS)? })
    }
    pub fn binding(&self) -> &ConnectBinding { &self.binding }
    pub(crate) fn identity(&self) -> Arc<()> { self.identity.clone() }
}

/// Retains actual host authority/inventory custody. Implementations check their
/// original owner/revision/clocks and never mint or renew a grant here. Calls
/// can block: fixed caller workers remain owned until their ACTUAL joins.
/// The independent watchdog uses only ConnectValidation's immutable bounds.
pub trait ConnectValidationGuard: Send + Sync + 'static {
    fn check_current(&self) -> Result<()>;
}

/// Neither Clone nor Serde. Runtime retains an Arc to this actual result, not
/// its address list or a receiver-created expiry. Time errors are absorbing.
pub struct ConnectValidation {
    query:ConnectQuery,
    grant:ConnectDeadline,
    inventory:ConnectDeadline,
    producer_current_until:ConnectDeadline,
    addresses:HostIpv4DenySet,
    owner:Arc<dyn ConnectValidationGuard>,
    last:Mutex<Sample>,
    denied:AtomicBool,
}
impl std::fmt::Debug for ConnectValidation {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("ConnectValidation([retained custody])")
    }
}
impl ConnectValidation {
    /// Trusted Rust adapter seam only. Retain the original native validation as
    /// owner; do not wrap JSON, a boolean, inventory metadata or grant expiry as
    /// live authority. Also retain ANY stricter original producer validation
    /// cutoff (for example native's current 250 ms query-as-lifetime contract).
    /// Caller retains its issuance/lifecycle owner separately.
    pub fn new(query:ConnectQuery, addresses:HostIpv4DenySet, grant:ConnectDeadline,
        inventory:ConnectDeadline, producer_current_until:ConnectDeadline,
        owner:Arc<dyn ConnectValidationGuard>) -> Result<Self> {
        let last = Mutex::new(query.started);
        let value = Self { query, grant, inventory, producer_current_until, addresses, owner, last, denied:AtomicBool::new(false) };
        value.check_adoption()?;
        Ok(value)
    }
    pub(crate) fn matches(&self, binding: &ConnectBinding, identity: &Arc<()>) -> bool {
        self.query.binding == *binding && Arc::ptr_eq(&self.query.identity, identity)
    }
    fn sample(&self, adoption: bool) -> Result<()> {
        let result = (|| {
            if self.denied.load(Ordering::Acquire) { return Err(Error::new(Kind::AuthorityDenied)); }
            // Only clock syscalls/arithmetic under this mutex; NEVER callbacks.
            let mut last = self.last.lock().map_err(|_|Error::new(Kind::AuthorityDenied))?;
            let now = Sample::now()?;
            if now.wall_ms < last.wall_ms || now.boot_ns < last.boot_ns {
                return Err(Error::new(Kind::AuthorityDenied));
            }
            self.grant.check(now)?; self.inventory.check(now)?;
            self.producer_current_until.check(now)?; self.query.stale_at.check(now)?;
            if adoption { self.query.adopt_by.check(now)?; }
            *last = now;
            Ok(())
        })();
        if result.is_err() { self.denied.store(true, Ordering::Release); }
        result
    }
    /// No arbitrary callback or authority IO: safe for the independent socket
    /// expiry watchdog even while a validation callback is still blocked.
    pub(crate) fn check_bounds(&self) -> Result<()> { self.sample(false) }
    pub(crate) fn check_adoption_bounds(&self) -> Result<()> { self.sample(true) }
    pub(crate) fn check_adoption(&self) -> Result<&HostIpv4DenySet> {
        self.checked(true)
    }
    pub fn check_current(&self) -> Result<&HostIpv4DenySet> { self.checked(false) }
    fn checked(&self, adoption: bool) -> Result<&HostIpv4DenySet> {
        let result = (|| {
            self.sample(adoption)?;
            self.owner.check_current()?; // No engine IO/clock mutex is held.
            self.sample(adoption)?; // Callback delay cannot manufacture time.
            Ok(&self.addresses)
        })();
        if result.is_err() { self.denied.store(true, Ordering::Release); }
        result
    }
}

// Explicit standalone qualifier/test fixture only; not a public/native inventory
// constructor. Its synthetic finite lifetime starts BEFORE any callback.
pub(super) fn qualifier_deadline(ms:i64) -> ConnectDeadline {
    assert!((1..=60_000).contains(&ms), "qualification-only finite lifetime");
    ConnectDeadline::after(Sample::now().unwrap(), ms).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("validity_tests.rs");
}
