//! A synchronous attempt's cancellation/publication boundary. Cancellation is
//! acknowledged only before publication begins; a completed publication is a
//! separate fact and cannot be retroactively turned into a cancelled attempt.
use anyhow::{ensure, Result};
use std::{sync::Mutex, time::{Duration, Instant}};

pub struct BuildExecutionControl {
    deadline: Instant,
    state: Mutex<State>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum State { Active, Cancelled, PublicationStarted, Published }
impl BuildExecutionControl {
    pub fn new(maximum: Duration) -> Result<Self> {
        ensure!(!maximum.is_zero() && maximum <= Duration::from_secs(3600), "invalid managed build deadline");
        Ok(Self { deadline: Instant::now() + maximum, state: Mutex::new(State::Active) })
    }
    /// True acknowledges cancellation before publication. False means the
    /// publication started or already completed. An error after publication
    /// starts requires reconciliation rather than a cancellation acknowledgment.
    /// Callers still retain this attempt until the
    /// synchronous build returns and its original subprocesses settle.
    pub fn cancel(&self) -> Result<bool> {
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("build control poisoned"))?;
        if matches!(*state, State::PublicationStarted | State::Published) { return Ok(false); }
        *state = State::Cancelled;
        Ok(true)
    }
    pub(super) fn check(&self) -> Result<()> {
        let state = self.state.lock().map_err(|_| anyhow::anyhow!("build control poisoned"))?;
        ensure!(*state == State::Active && Instant::now() < self.deadline, "managed build cancelled or deadline expired");
        Ok(())
    }
    pub(super) fn publish<T>(&self, publish: impl FnOnce() -> Result<T>) -> Result<T> {
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("build control poisoned"))?;
        ensure!(*state == State::Active && Instant::now() < self.deadline, "managed build cancelled or deadline expired before publication");
        // A storage error can be an uncertain commit. Once the effect starts,
        // cancellation must never assert that publication was prevented.
        *state = State::PublicationStarted;
        let result = publish()?;
        *state = State::Published;
        ensure!(Instant::now() < self.deadline,
            "managed build publication completed after its deadline; reconcile the published result");
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acknowledged_cancel_never_executes_publication() {
        let control = BuildExecutionControl::new(Duration::from_secs(10)).unwrap();
        assert!(control.cancel().unwrap());
        assert!(control.publish(|| -> Result<()> { panic!("cancelled publication ran") }).is_err());
        assert!(control.check().is_err());
    }
    #[test]
    fn completed_publication_cannot_acknowledge_a_later_cancel() {
        let control = BuildExecutionControl::new(Duration::from_secs(10)).unwrap();
        assert_eq!(control.publish(|| Ok(42)).unwrap(), 42);
        assert!(!control.cancel().unwrap());
    }
    #[test]
    fn failed_publication_requires_reconciliation_instead_of_cancel_ack() {
        let control = BuildExecutionControl::new(Duration::from_secs(10)).unwrap();
        assert!(control.publish(|| -> Result<()> { anyhow::bail!("failed") }).is_err());
        assert!(!control.cancel().unwrap());
        assert!(control.check().is_err());
    }
    #[test]
    fn late_publication_reports_unknown_completion_without_false_success_or_cancel_ack() {
        let control=BuildExecutionControl::new(Duration::from_millis(20)).unwrap();
        assert!(control.publish(|| { std::thread::sleep(Duration::from_millis(30)); Ok(42) }).is_err());
        assert!(!control.cancel().unwrap(),"actual publication cannot be erased by a late cancellation");
    }
}
