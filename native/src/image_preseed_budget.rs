//! A separate once-per-image original clock. Never extends a task/startup clock.
use anyhow::{ensure, Result};
use rudder_engine::fleet_lease::{HostClock, SuspendAwareClock};
use serde::{Deserialize, Serialize};
use std::{sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::Duration};

pub const IMAGE_PRESEED_LIMIT: Duration = Duration::from_secs(300);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePreseedConstraint {
    format_version: u32,
    maximum_milliseconds: u32,
    boottime_deadline_nanoseconds: String,
}
impl ImagePreseedConstraint {
    fn deadline(&self) -> Result<u64> {
        let s=&self.boottime_deadline_nanoseconds;
        ensure!(self.format_version==1 && (1..=300_000).contains(&self.maximum_milliseconds)
            && !s.is_empty() && s.len()<=20 && !s.starts_with('0') && s.bytes().all(|b|b.is_ascii_digit()),
            "image preseed time constraint invalid");
        Ok(s.parse()?)
    }
}
struct Shared { clock: HostClock, constraint: ImagePreseedConstraint, last: Mutex<u64>, cancelled: AtomicBool }
/// Non-Serde process-local clock custody; clones share the original cutoff and
/// absorbing cancellation. A constraint by itself is DATA, not upload authority.
#[derive(Clone)]
pub struct ImagePreseedBudget(Arc<Shared>);
impl ImagePreseedBudget {
    /// Capture BEFORE image capture/queueing, never once per phase/object/chunk.
    pub fn begin(maximum: Duration) -> Result<Self> {
        ensure!(!maximum.is_zero() && maximum<=IMAGE_PRESEED_LIMIT,"image preseed time exceeds policy");
        let ms=u32::try_from(maximum.as_millis())?; ensure!(ms>0,"image preseed time below precision");
        let clock=HostClock::new().map_err(|_|anyhow::anyhow!("image preseed clock unavailable"))?;
        let now=sample(&clock)?; let end=now.checked_add(u64::from(ms)*1_000_000).ok_or_else(||anyhow::anyhow!("image preseed clock overflow"))?;
        Ok(Self(Arc::new(Shared { clock, constraint:ImagePreseedConstraint {format_version:1,maximum_milliseconds:ms,
            boottime_deadline_nanoseconds:end.to_string()},last:Mutex::new(now),cancelled:AtomicBool::new(false) })))
    }
    // Only the authenticated original Root/native operation receiver uses this.
    pub(super) fn adopt(constraint: ImagePreseedConstraint) -> Result<Self> {
        let end=constraint.deadline()?;let clock=HostClock::new().map_err(|_|anyhow::anyhow!("image preseed clock unavailable"))?;
        let now=sample(&clock)?;
        ensure!(now<end && end-now<=u64::from(constraint.maximum_milliseconds)*1_000_000,"image preseed original clock expired or different");
        Ok(Self(Arc::new(Shared {clock,constraint,last:Mutex::new(now),cancelled:AtomicBool::new(false)})))
    }
    pub fn constraint(&self)->ImagePreseedConstraint { self.0.constraint.clone() }
    pub fn cancel(&self) { self.0.cancelled.store(true,Ordering::Release); }
    pub fn remaining(&self)->Result<Duration> {
        let result=(|| {ensure!(!self.0.cancelled.load(Ordering::Acquire),"image preseed clock fenced");
            let end=self.0.constraint.deadline()?;ensure!(sample(&self.0.clock)?<end,"image preseed clock expired");
            let mut last=self.0.last.lock().map_err(|_|anyhow::anyhow!("image preseed clock unavailable"))?;
            let now=sample(&self.0.clock)?;
            ensure!(now>=*last && now<end,"image preseed clock expired or regressed");*last=now;
            ensure!(!self.0.cancelled.load(Ordering::Acquire),"image preseed clock fenced");Ok(Duration::from_nanos(end-now))})();
        if result.is_err(){self.cancel();}result
    }
}
fn sample(clock:&HostClock)->Result<u64> {
    Ok(u64::try_from(clock.read().map_err(|_|anyhow::anyhow!("image preseed clock unavailable"))?.nanoseconds())?)
}

#[cfg(test)]
#[path="image_preseed_budget_contention_tests.rs"]
mod contention_tests;
