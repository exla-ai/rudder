//! Original Spec5 startup time, carried through the sealed helper and child.
//! This DATA constrains admission only. It creates no owner, grant or process
//! authority, and it is never renewed at a handoff or backend spawn.
use super::finalization_budget::{FinalizationBudget, FinalizationConstraint};
use anyhow::{anyhow, ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const STARTUP_POLICY_LIMIT: Duration = Duration::from_secs(30);
const MAX_JS_INTEGER: i64 = 9_007_199_254_740_991;
type WallClock = Arc<dyn Fn() -> Result<i64> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupConstraint {
    original: FinalizationConstraint,
    started_at_ms: i64,
}
impl StartupConstraint {
    /// Bind the original suspend-aware budget to the same wall-clock start
    /// used for fixed guest/host grant expiry. Neither clock may add time.
    pub fn from_original(original: FinalizationConstraint, started_at_ms: i64) -> Result<Self> {
        let value = Self { original, started_at_ms };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        let maximum = self.original.maximum()?;
        ensure!(maximum <= STARTUP_POLICY_LIMIT, "source startup exceeds policy");
        let maximum_ms = i64::try_from(maximum.as_millis())?;
        ensure!(
            self.started_at_ms >= 0
                && self.started_at_ms.checked_add(maximum_ms)
                    .is_some_and(|end| end <= MAX_JS_INTEGER),
            "invalid source startup wall-clock interval"
        );
        Ok(())
    }
    /// Convenience for trusted fixtures/ingress that do not already own a
    /// preparation budget. Actual source preparation uses from_original.
    pub fn begin(maximum: Duration) -> Result<Self> {
        let wall = wall_ms()?;
        Self::from_original(FinalizationBudget::begin(maximum)?.constraint(), wall)
    }
}

fn wall_ms() -> Result<i64> {
    Ok(i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?)
}

/// Owned local clock checks only. Clones share absorbing clock failure, while
/// crossing a process boundary preserves the original absolute constraints.
#[derive(Clone)]
pub struct StartupBudget {
    constraint: StartupConstraint,
    original: FinalizationBudget,
    wall: WallClock,
    last_wall: Arc<Mutex<i64>>,
}

/// Immutable time DATA captured by the parent after its absorbing budget check.
/// No locks, callbacks, allocation or renewed time enter the pre-exec child.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
pub(super) struct ExecAdmissionSnapshot {
    boottime_deadline_ns: u64,
    last_boottime_ns: u64,
    wall_start_ms: i64,
    wall_deadline_ms: i64,
    last_wall_ms: i64,
}
#[cfg(target_os = "linux")]
impl ExecAdmissionSnapshot {
    fn permits(&self, boottime_ns: u64, wall_ms: i64) -> bool {
        boottime_ns >= self.last_boottime_ns && boottime_ns < self.boottime_deadline_ns
            && wall_ms >= self.wall_start_ms && wall_ms >= self.last_wall_ms
            && wall_ms < self.wall_deadline_ms
    }
    /// Called only after fork. clock_gettime, integer operations and a raw OS
    /// error keep both success and refusal paths async-signal-safe.
    pub(super) fn check_before_exec(&self) -> std::io::Result<()> {
        let mut boot = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let mut wall = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let read = unsafe {
            libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut boot) == 0
                && libc::clock_gettime(libc::CLOCK_REALTIME, &mut wall) == 0
        };
        let allowed = read && match (timespec_parts(boot), timespec_parts(wall)) {
            (Some((boot_s, boot_ns)), Some((wall_s, wall_ns))) => {
                let boottime = boot_s.checked_mul(1_000_000_000)
                    .and_then(|n| n.checked_add(boot_ns));
                let realtime = wall_s.checked_mul(1_000)
                    .and_then(|n| n.checked_add(wall_ns / 1_000_000))
                    .and_then(|n| i64::try_from(n).ok());
                match (boottime, realtime) {
                    (Some(boot), Some(wall)) => self.permits(boot, wall),
                    _ => false,
                }
            }
            _ => false,
        };
        if allowed { Ok(()) } else {
            Err(std::io::Error::from_raw_os_error(libc::ETIMEDOUT))
        }
    }
}
#[cfg(target_os = "linux")]
fn timespec_parts(value: libc::timespec) -> Option<(u64, u64)> {
    let seconds = u64::try_from(value.tv_sec).ok()?;
    let nanos = u64::try_from(value.tv_nsec).ok()?;
    if nanos >= 1_000_000_000 { return None; }
    Some((seconds, nanos))
}
impl super::ExecutionSpec {
    /// The sealed Spec5 startup clock is mandatory even when callers reach a
    /// lower-level child entrypoint. Older families have no startup constraint.
    pub fn startup_budget(&self) -> Result<Option<StartupBudget>> {
        if self.format_version != 5 { return Ok(None); }
        let constraint = self.profile.as_ref().and_then(|profile| profile.startup.as_ref())
            .ok_or_else(|| anyhow!("source specification requires its original startup constraint"))?;
        Ok(Some(StartupBudget::from_constraint(constraint)?))
    }
}
impl StartupBudget {
    pub fn from_constraint(constraint: &StartupConstraint) -> Result<Self> {
        Self::from_parts(constraint, FinalizationBudget::from_constraint(&constraint.original)?,
            Arc::new(wall_ms))
    }
    fn from_parts(constraint: &StartupConstraint, original: FinalizationBudget, wall: WallClock) -> Result<Self> {
        constraint.validate()?;
        ensure!(original.constraint() == constraint.original, "source startup clock changed");
        let value = Self { constraint: constraint.clone(), original, wall,
            last_wall: Arc::new(Mutex::new(constraint.started_at_ms)) };
        value.remaining()?;
        Ok(value)
    }
    pub fn remaining(&self) -> Result<Duration> {
        let result = (|| {
            let mut last = self.last_wall.lock().map_err(|_| anyhow!("source startup clock unavailable"))?;
            let remaining = self.original.remaining()?;
            let now = (self.wall)()?;
            let end = self.constraint.started_at_ms
                .checked_add(i64::try_from(self.constraint.original.maximum()?.as_millis())?)
                .ok_or_else(|| anyhow!("source startup clock overflow"))?;
            ensure!(now >= *last && now < end, "source startup expired or wall clock regressed");
            *last = now;
            Ok(remaining.min(Duration::from_millis(u64::try_from(end - now)?)))
        })();
        if result.is_err() { self.original.cancel(); }
        result
    }
    pub fn phase_limit(&self, maximum: Duration) -> Result<Duration> {
        ensure!(!maximum.is_zero(), "zero source startup phase");
        Ok(maximum.min(self.remaining()?))
    }

    /// Parent-only capture after cancellation/clock checks. Original deadlines
    /// and last samples are copied directly, with no new admission interval.
    #[cfg(target_os = "linux")]
    pub(super) fn exec_admission_snapshot(&self) -> Result<ExecAdmissionSnapshot> {
        self.remaining()?;
        let result = (|| {
            let last_wall = self.last_wall.lock()
                .map_err(|_| anyhow!("source startup clock unavailable"))?;
            let (boottime_deadline_ns, last_boottime_ns) = self.original.admission_clock_bounds()?;
            let wall_deadline_ms = self.constraint.started_at_ms
                .checked_add(i64::try_from(self.constraint.original.maximum()?.as_millis())?)
                .ok_or_else(|| anyhow!("source startup clock overflow"))?;
            Ok(ExecAdmissionSnapshot {
                boottime_deadline_ns, last_boottime_ns,
                wall_start_ms: self.constraint.started_at_ms,
                wall_deadline_ms, last_wall_ms: *last_wall,
            })
        })();
        if result.is_err() { self.original.cancel(); }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixture() -> (Arc<AtomicU64>, Arc<AtomicU64>, StartupConstraint, StartupBudget) {
        let boot = Arc::new(AtomicU64::new(1_000_000_000));
        let wall = Arc::new(AtomicU64::new(1_000));
        let sample = boot.clone();
        let original = FinalizationBudget::begin_with_clock(STARTUP_POLICY_LIMIT,
            Arc::new(move || Ok(u128::from(sample.load(Ordering::Acquire))))).unwrap();
        let constraint = StartupConstraint::from_original(original.constraint(), 1_000).unwrap();
        let sample = wall.clone();
        let budget = StartupBudget::from_parts(&constraint, original,
            Arc::new(move || Ok(i64::try_from(sample.load(Ordering::Acquire))?))).unwrap();
        (boot, wall, constraint, budget)
    }
    #[test]
    fn delayed_source_preparation_and_child_admission_keep_the_original_deadline() {
        let (boot, wall, constraint, budget) = fixture();
        boot.store(29_000_000_000, Ordering::Release);
        wall.store(29_000, Ordering::Release);
        assert_eq!(budget.phase_limit(Duration::from_secs(10)).unwrap(), Duration::from_secs(2));
        // Crossing the sealed child handoff cannot turn the remaining two
        // seconds into another thirty-second preparation allowance.
        let sample = boot.clone();
        let child = FinalizationBudget::adopt_with_clock(constraint.original.clone(),
            Arc::new(move || Ok(u128::from(sample.load(Ordering::Acquire))))).unwrap();
        let sample = wall.clone();
        let child = StartupBudget::from_parts(&constraint, child,
            Arc::new(move || Ok(i64::try_from(sample.load(Ordering::Acquire))?))).unwrap();
        boot.store(30_999_999_999, Ordering::Release);
        wall.store(30_999, Ordering::Release);
        assert_eq!(child.remaining().unwrap(), Duration::from_nanos(1));
        boot.store(31_000_000_000, Ordering::Release);
        assert!(child.remaining().is_err());
        assert!(budget.remaining().is_err());
        boot.store(30_000_000_000, Ordering::Release);
        assert!(child.remaining().is_err());
        assert!(budget.remaining().is_err());
    }
    #[test]
    fn wall_expiry_or_regression_fences_admission_even_with_live_boottime() {
        for regressed in [false, true] {
            let (_boot, wall, _constraint, budget) = fixture();
            wall.store(if regressed { 999 } else { 31_000 }, Ordering::Release);
            assert!(budget.remaining().is_err());
            wall.store(1_000, Ordering::Release);
            assert!(budget.remaining().is_err());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn exec_snapshot_keeps_original_absolute_deadlines_and_refuses_regressed_samples() {
        let (boot, wall, _constraint, budget) = fixture();
        boot.store(29_000_000_000, Ordering::Release);
        wall.store(29_000, Ordering::Release);
        let snapshot = budget.exec_admission_snapshot().unwrap();
        assert_eq!(snapshot.boottime_deadline_ns, 31_000_000_000);
        assert_eq!(snapshot.wall_start_ms, 1_000);
        assert_eq!(snapshot.wall_deadline_ms, 31_000);
        assert_eq!(snapshot.last_boottime_ns, 29_000_000_000);
        assert_eq!(snapshot.last_wall_ms, 29_000);
        assert!(snapshot.permits(30_999_999_999, 30_999));
        assert!(!snapshot.permits(31_000_000_000, 30_999));
        assert!(!snapshot.permits(30_000_000_000, 31_000));
        assert!(!snapshot.permits(28_999_999_999, 30_000));
        assert!(!snapshot.permits(30_000_000_000, 28_999));
        budget.original.cancel();
        assert!(budget.exec_admission_snapshot().is_err(),
            "parent cancellation must refuse snapshot capture");
    }
}
