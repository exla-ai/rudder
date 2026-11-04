//! Suspend-aware host clock used by bounded finalization budgets.

use uuid::Uuid;

const NS_PER_MS: u128 = 1_000_000;

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
