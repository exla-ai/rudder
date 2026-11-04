//! Process replacement under the caller's original exclusive checkout lock.
//! Reconnecting surfaces never enter this module. Reports carry observations,
//! never process custody or permission to relaunch quarantined work.
use super::*;
use restart::{ExpectedRuntimeAttempt, NativeRestartRecovery};

pub(crate) struct RestartBlocked {
    pub recovery: NativeRestartRecovery,
    cause: String,
}
impl std::fmt::Debug for RestartBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {std::fmt::Display::fmt(self, f)}
}
impl std::fmt::Display for RestartBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original owner replacement requires reconciliation: {}", self.cause)?;
        // Identifiers only. Never format command payloads, results or tokens
        // into the host's product error. The retained typed report has the
        // bounded receipt pages needed by trusted reconciliation tooling.
        for receipt in self.recovery.effects.uncertain.iter().take(16) {
            write!(f, "; uncertain command {}", receipt.command_id)?;
        }
        for runtime in self.recovery.runtimes.iter().filter(|runtime|matches!(runtime,
            restart::RecoveredRuntimeDisposition::Quarantined {..})).take(16) {
            if let restart::RecoveredRuntimeDisposition::Quarantined {attempt,reason}=runtime {
                write!(f, "; quarantined runtime {} epoch {} attempt {} ({reason:?})",
                    attempt.run_id,attempt.run_epoch,attempt.attempt_id)?;
            }
        }
        Ok(())
    }
}
impl std::error::Error for RestartBlocked {}

/// Failed ordinary acquisition retains the same physical checkout lock and
/// journal. The caller may inspect the typed cause and reconcile the original
/// epoch; a new lock, PID or journal flag cannot replace this custody.
pub(crate) struct RetainedRestartFailure {
    cause: anyhow::Error,
    original: std::sync::Mutex<(CheckoutLock, Journal, OwnerFence)>,
}
impl RetainedRestartFailure {
    pub(super) fn new(cause: anyhow::Error, lock: CheckoutLock, journal: Journal, owner: OwnerFence) -> Self {
        Self {cause, original: std::sync::Mutex::new((lock, journal, owner))}
    }
    pub(crate) fn recovery(&self) -> Option<&NativeRestartRecovery> {
        self.cause.downcast_ref::<RestartBlocked>().map(|blocked|&blocked.recovery)
    }
    pub(crate) fn reconcile<T>(&self, reconcile: impl FnOnce(&mut Journal, &OwnerFence) -> Result<T>) -> Result<T> {
        let mut original = self.original.lock().map_err(|_|anyhow::anyhow!("original restart custody is poisoned"))?;
        let (_, journal, owner) = &mut *original;
        reconcile(journal, owner)
    }
}
impl std::fmt::Debug for RetainedRestartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {std::fmt::Display::fmt(self, f)}
}
impl std::fmt::Display for RetainedRestartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {std::fmt::Display::fmt(&self.cause, f)}
}
impl std::error::Error for RetainedRestartFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {Some(self.cause.as_ref())}
}

/// The caller retains its actual lock and journal throughout this operation,
/// including on error. Inspect RestartBlocked to reconcile real external work
/// against the original epoch; retrying does not settle uncertain custody.
pub(super) fn replace_locked_owner(
    journal: &mut Journal,
    previous: &OwnerFence,
    next_owner: &str,
    now_ms: i64,
    report: &mut Option<NativeRestartRecovery>,
) -> Result<OwnerFence> {
    let state = journal.project_state(&previous.project_id)?;
    anyhow::ensure!(state.owner == *previous, "recorded owner changed before restart");
    if state.owner_active {
        let mut expected = journal.recorded_runtime_inventory(previous)?.into_iter().map(|runtime| {
            (runtime.run_id.clone(), ExpectedRuntimeAttempt {run_id: runtime.run_id, run_epoch: runtime.run_epoch,
                attempt_id: runtime.attempt_id})
        }).collect::<std::collections::BTreeMap<_,_>>();
        for (run_id,run_epoch,attempt_id) in journal.recorded_runtime_attempt_inventory(previous)? {
            // A reserved attempt can crash before its first observation. An
            // old terminal observation for a different instance cannot cover
            // it. Same-instance terminal rows may predate an owner epoch bump.
            if expected.get(&run_id).is_none_or(|held|held.attempt_id != attempt_id) {
                expected.insert(run_id.clone(), ExpectedRuntimeAttempt {run_id,run_epoch,attempt_id});
            }
        }
        let expected=expected.into_values().collect::<Vec<_>>();
        *report = Some(restart::recover_before_replacement(journal, previous, &expected, now_ms)?);
    }
    if let Some(recovery) = report.as_ref() {
        if recovery.runtimes.iter().any(|runtime|matches!(runtime, restart::RecoveredRuntimeDisposition::Quarantined {..})) {
            return Err(RestartBlocked {recovery: recovery.clone(),
                cause: "native runtime inventory requires physical custody reconciliation".into()}.into());
        }
    }
    match journal.transfer_recovered_owner(previous, next_owner, now_ms) {
        Ok(fence) => Ok(fence),
        Err(error) => match report.as_ref() {
            Some(recovery) => Err(RestartBlocked {recovery: recovery.clone(), cause: format!("{error:#}")}.into()),
            None => Err(error),
        },
    }
}
