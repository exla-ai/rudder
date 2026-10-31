//! One suspend-aware deadline shared by every finalization phase.
//!
//! A constraint limits time only. It does not authenticate a shutdown request,
//! retain a source grant, stop a process, settle a worker or prove a checkpoint.
//! Callers must keep the actual owners until those independent effects settle.
use crate::fleet_lease::{HostClock, SuspendAwareClock};
use anyhow::{anyhow, ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const MAX_MILLISECONDS: u32 = 30_000;
pub const FINALIZATION_POLICY_LIMIT: Duration = Duration::from_millis(MAX_MILLISECONDS as u64);
const MAX_CONSTRAINT_BYTES: usize = 256;
const NS_PER_MS: u128 = 1_000_000;
type Clock = Arc<dyn Fn() -> Result<u128> + Send + Sync>;

fn fault() -> anyhow::Error {
    anyhow!("execution finalization budget unavailable")
}

/// Serialized time bounds, never a live execution or drain capability. The
/// decimal string preserves nanoseconds through clients with JSON number limits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizationConstraint {
    format_version: u32,
    maximum_milliseconds: u32,
    boottime_deadline_nanoseconds: String,
}

impl FinalizationConstraint {
    pub(super) fn maximum(&self) -> Result<Duration> {
        self.deadline()?;
        Ok(Duration::from_millis(u64::from(self.maximum_milliseconds)))
    }
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_CONSTRAINT_BYTES,
            "finalization constraint exceeds bound"
        );
        let value: Self = serde_json::from_slice(bytes).map_err(|_| fault())?;
        value.deadline()?;
        Ok(value)
    }

    fn deadline(&self) -> Result<u128> {
        let text = &self.boottime_deadline_nanoseconds;
        ensure!(
            self.format_version == 1
                && (1..=MAX_MILLISECONDS).contains(&self.maximum_milliseconds)
                && !text.is_empty()
                && text.len() <= 20
                && !text.starts_with('0')
                && text.bytes().all(|byte| byte.is_ascii_digit()),
            "invalid finalization constraint"
        );
        Ok(text.parse::<u64>().map_err(|_| fault())? as u128)
    }
}

struct SharedBudget {
    clock: Clock,
    constraint: FinalizationConstraint,
    deadline: u128,
    last: Mutex<u128>,
    fenced: AtomicBool,
}

/// Process-local time accounting, with no serialized constructor for an owned
/// execution. Clones share the original deadline and absorbing cancellation.
/// Production uses HostClock; Fly pause/restore continuity needs its own gate.
#[derive(Clone)]
pub struct FinalizationBudget(Arc<SharedBudget>);

impl FinalizationBudget {
    pub fn begin(maximum: Duration) -> Result<Self> {
        Self::begin_with_clock(maximum, Self::host_clock()?)
    }

    /// Adopt only a time constraint after the caller independently authenticates
    /// its owned control request. Both processes must use the same live kernel
    /// clock. A saved constraint creates no authority and cannot renew its time.
    pub fn from_constraint(constraint: &FinalizationConstraint) -> Result<Self> {
        Self::adopt_with_clock(constraint.clone(), Self::host_clock()?)
    }

    pub fn constraint(&self) -> FinalizationConstraint {
        self.0.constraint.clone()
    }

    pub fn cancel(&self) {
        self.0.fenced.store(true, Ordering::Release);
    }

    /// Sample immediately before every wait/effect. A phase does not receive a
    /// new budget; elapsed queue, transport and earlier phase time stays charged.
    pub fn remaining(&self) -> Result<Duration> {
        let mut last = match self.0.last.lock() {
            Ok(last) => last,
            Err(_) => {
                self.cancel();
                return Err(fault());
            }
        };
        let result = (|| {
            ensure!(
                !self.0.fenced.load(Ordering::Acquire),
                "finalization budget retired"
            );
            let now = (self.0.clock)().map_err(|_| fault())?;
            ensure!(
                now >= *last && now < self.0.deadline,
                "finalization deadline expired or clock regressed"
            );
            *last = now;
            let nanos = u64::try_from(self.0.deadline - now).map_err(|_| fault())?;
            ensure!(
                !self.0.fenced.load(Ordering::Acquire),
                "finalization budget retired"
            );
            Ok(Duration::from_nanos(nanos))
        })();
        if result.is_err() {
            self.cancel();
        }
        result
    }

    pub fn phase_limit(&self, upper_bound: Duration) -> Result<Duration> {
        ensure!(!upper_bound.is_zero(), "zero finalization phase bound");
        Ok(self.remaining()?.min(upper_bound))
    }

    /// Capture checked absolute clock data before fork. The child must never
    /// reconstruct this deadline from a later clock sample plus remaining time.
    #[cfg(target_os = "linux")]
    pub(super) fn admission_clock_bounds(&self) -> Result<(u64, u64)> {
        self.remaining()?;
        let result = (|| {
            let last = self.0.last.lock().map_err(|_| fault())?;
            ensure!(!self.0.fenced.load(Ordering::Acquire), "finalization budget retired");
            Ok((u64::try_from(self.0.deadline).map_err(|_| fault())?,
                u64::try_from(*last).map_err(|_| fault())?))
        })();
        if result.is_err() { self.cancel(); }
        result
    }

    fn host_clock() -> Result<Clock> {
        let clock = HostClock::new().map_err(|_| fault())?;
        Ok(Arc::new(move || {
            clock
                .read()
                .map(|reading| reading.nanoseconds())
                .map_err(|_| fault())
        }))
    }

    pub(super) fn begin_with_clock(maximum: Duration, clock: Clock) -> Result<Self> {
        ensure!(
            !maximum.is_zero() && maximum <= FINALIZATION_POLICY_LIMIT,
            "finalization budget exceeds bounded policy"
        );
        let milliseconds = u32::try_from(maximum.as_millis()).map_err(|_| fault())?;
        ensure!(
            milliseconds > 0,
            "finalization budget is below clock policy precision"
        );
        let now = clock().map_err(|_| fault())?;
        // Round the requested interval down; serialization must never add time.
        let deadline = now
            .checked_add(milliseconds as u128 * NS_PER_MS)
            .ok_or_else(fault)?;
        let deadline = u64::try_from(deadline).map_err(|_| fault())?;
        let constraint = FinalizationConstraint {
            format_version: 1,
            maximum_milliseconds: milliseconds,
            boottime_deadline_nanoseconds: deadline.to_string(),
        };
        Self::from_sample(constraint, clock, now)
    }

    pub(super) fn adopt_with_clock(constraint: FinalizationConstraint, clock: Clock) -> Result<Self> {
        let now = clock().map_err(|_| fault())?;
        Self::from_sample(constraint, clock, now)
    }

    fn from_sample(constraint: FinalizationConstraint, clock: Clock, now: u128) -> Result<Self> {
        let deadline = constraint.deadline()?;
        ensure!(
            deadline > now && deadline - now <= constraint.maximum_milliseconds as u128 * NS_PER_MS,
            "finalization constraint expired or differs from host clock"
        );
        Ok(Self(Arc::new(SharedBudget {
            clock,
            constraint,
            deadline,
            last: Mutex::new(now),
            fenced: AtomicBool::new(false),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn fixture(now: u64, milliseconds: u64) -> (Arc<AtomicU64>, FinalizationBudget) {
        let value = Arc::new(AtomicU64::new(now));
        let source = value.clone();
        let clock = Arc::new(move || Ok(source.load(Ordering::Acquire) as u128));
        let budget =
            FinalizationBudget::begin_with_clock(Duration::from_millis(milliseconds), clock)
                .unwrap();
        (value, budget)
    }

    #[test]
    fn delayed_delivery_and_later_phases_cannot_restart_the_original_deadline() {
        let (clock, parent) = fixture(1_000_000_000, 30_000);
        let constraint = parent.constraint();
        clock.store(11_000_000_000, Ordering::Release);
        let sampled = clock.clone();
        let helper = FinalizationBudget::adopt_with_clock(
            constraint.clone(),
            Arc::new(move || Ok(sampled.load(Ordering::Acquire) as u128)),
        )
        .unwrap();
        assert_eq!(helper.remaining().unwrap(), Duration::from_secs(20));
        clock.store(30_000_000_000, Ordering::Release);
        assert_eq!(
            helper.phase_limit(Duration::from_secs(35)).unwrap(),
            Duration::from_secs(1)
        );
        assert_eq!(parent.remaining().unwrap(), Duration::from_secs(1));
        clock.store(31_000_000_000, Ordering::Release);
        assert!(parent.remaining().is_err());
        assert!(helper.remaining().is_err());
        let sampled = clock.clone();
        assert!(FinalizationBudget::adopt_with_clock(
            constraint,
            Arc::new(move || Ok(sampled.load(Ordering::Acquire) as u128))
        )
        .is_err());
    }

    #[test]
    fn regressions_faults_and_cancellation_are_absorbing_across_clones() {
        let (clock, budget) = fixture(1_000_000_000, 30_000);
        let copy = budget.clone();
        clock.store(999_999_999, Ordering::Release);
        assert!(budget.remaining().is_err());
        clock.store(2_000_000_000, Ordering::Release);
        assert!(copy.remaining().is_err());
        let (_, budget) = fixture(1, 30_000);
        let copy = budget.clone();
        budget.cancel();
        assert!(copy.remaining().is_err());
        let calls = Arc::new(AtomicU64::new(0));
        let sampled = calls.clone();
        let faulty: Clock = Arc::new(move || {
            if sampled.fetch_add(1, Ordering::AcqRel) == 0 {
                Ok(1)
            } else {
                Err(fault())
            }
        });
        let budget = FinalizationBudget::begin_with_clock(Duration::from_secs(1), faulty).unwrap();
        assert!(budget.remaining().is_err());
        assert!(budget.remaining().is_err());
        assert_eq!(calls.load(Ordering::Acquire), 2);
    }

    #[test]
    fn wire_bounds_are_canonical_and_submillisecond_rounding_never_adds_time() {
        let (_, budget) = fixture(9_007_199_254_740_993, 30_000);
        let bytes = serde_json::to_vec(&budget.constraint()).unwrap();
        assert_eq!(
            FinalizationConstraint::from_json(&bytes).unwrap(),
            budget.constraint()
        );
        for replacement in ["0", "01", "-1", "1.0", "18446744073709551616"] {
            let mut value = serde_json::to_value(budget.constraint()).unwrap();
            value["boottime_deadline_nanoseconds"] = replacement.into();
            assert!(
                FinalizationConstraint::from_json(&serde_json::to_vec(&value).unwrap()).is_err()
            );
        }
        let mut value = serde_json::to_value(budget.constraint()).unwrap();
        value["unexpected"] = true.into();
        assert!(FinalizationConstraint::from_json(&serde_json::to_vec(&value).unwrap()).is_err());
        assert!(FinalizationConstraint::from_json(&vec![b' '; MAX_CONSTRAINT_BYTES + 1]).is_err());
        let clock: Clock = Arc::new(|| Ok(1_000_000_000));
        let budget =
            FinalizationBudget::begin_with_clock(Duration::from_micros(1_999), clock.clone())
                .unwrap();
        assert_eq!(budget.remaining().unwrap(), Duration::from_millis(1));
        for duration in [
            Duration::ZERO,
            Duration::from_micros(999),
            Duration::from_secs(31),
        ] {
            assert!(FinalizationBudget::begin_with_clock(duration, clock.clone()).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn actual_stalled_io_consumes_budget_before_a_second_phase_can_start() {
        use std::{io::Read, os::unix::net::UnixStream, time::Instant};
        let (mut reader, _held_writer) = UnixStream::pair().unwrap();
        let timeout = Duration::from_millis(80);
        reader.set_read_timeout(Some(timeout)).unwrap();
        let budget = FinalizationBudget::begin(timeout).unwrap();
        let guard = Instant::now() + Duration::from_secs(2);
        let mut attempts = 0;
        loop {
            // Always perform the stalled read this test is about. Scheduling
            // can consume the short budget before the first loop condition;
            // late IO must still withhold permission for the next phase.
            let error = reader.read(&mut [0]).unwrap_err();
            assert!(matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
            ));
            attempts += 1;
            let Ok(remaining) = budget.remaining() else { break; };
            assert!(Instant::now() < guard, "actual host budget did not expire");
            reader.set_read_timeout(Some(remaining)).unwrap();
        }
        assert!(attempts > 0);
        assert!(budget.phase_limit(Duration::from_secs(35)).is_err());
        assert!(FinalizationBudget::from_constraint(&budget.constraint()).is_err());
    }
}
