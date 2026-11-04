//! Native startup projection. The parent supplies the existing engine journal
//! module so the same adapter is exercised by the engine's crash test harness.
//! No recovery disposition grants permission to spawn or resume a backend.
use super::recovery_journal::{Journal, OwnerFence, Recovery, RuntimePhase};
use anyhow::{ensure, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpectedRuntimeAttempt {
    pub run_id: String,
    pub run_epoch: u64,
    pub attempt_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeQuarantineReason {
    MissingObservation,
    ReplacedAttempt,
    UncertainCustody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveredRuntimeDisposition {
    Paused(ExpectedRuntimeAttempt),
    Completed(ExpectedRuntimeAttempt),
    Failed(ExpectedRuntimeAttempt),
    Quarantined {
        attempt: ExpectedRuntimeAttempt,
        reason: RuntimeQuarantineReason,
    },
}

pub(crate) struct NativeRestartRecovery {
    pub effects: Recovery,
    pub runtimes: Vec<RecoveredRuntimeDisposition>,
}

/// OFF App after reacquiring the original exclusive owner lock, before an
/// owner/attempt epoch change. A surface reconnect must never invoke this.
/// Callers preserve these dispositions in the App loop: Paused stays Paused
/// with no live process; quarantine retains original recovery and forbids an
/// automatic relaunch. Physical custody must be reconciled independently.
pub(crate) fn recover_before_replacement(
    journal: &mut Journal,
    owner: &OwnerFence,
    expected: &[ExpectedRuntimeAttempt],
    now_ms: i64,
) -> Result<NativeRestartRecovery> {
    ensure!(
        expected.len() <= 4096,
        "native runtime recovery exceeds4096 attempts"
    );
    let effects = journal.recover_runtime_restart(owner, now_ms)?;
    let mut runtimes = Vec::with_capacity(expected.len());
    for attempt in expected {
        let runtime = journal.runtime_snapshot(owner, &attempt.run_id, now_ms)?;
        let disposition = match runtime {
            None => RecoveredRuntimeDisposition::Quarantined {
                attempt: attempt.clone(),
                reason: RuntimeQuarantineReason::MissingObservation,
            },
            Some(runtime)
                if runtime.observation.run_epoch != attempt.run_epoch
                    || runtime.observation.attempt_id != attempt.attempt_id =>
            {
                RecoveredRuntimeDisposition::Quarantined {
                    attempt: attempt.clone(),
                    reason: RuntimeQuarantineReason::ReplacedAttempt,
                }
            }
            Some(runtime) if !runtime.observation.pty_owned => match runtime.observation.phase {
                RuntimePhase::Closed => RecoveredRuntimeDisposition::Paused(attempt.clone()),
                RuntimePhase::Completed => RecoveredRuntimeDisposition::Completed(attempt.clone()),
                RuntimePhase::Failed => RecoveredRuntimeDisposition::Failed(attempt.clone()),
                _ => RecoveredRuntimeDisposition::Quarantined {
                    attempt: attempt.clone(),
                    reason: RuntimeQuarantineReason::UncertainCustody,
                },
            },
            Some(_) => RecoveredRuntimeDisposition::Quarantined {
                attempt: attempt.clone(),
                reason: RuntimeQuarantineReason::UncertainCustody,
            },
        };
        runtimes.push(disposition);
    }
    Ok(NativeRestartRecovery { effects, runtimes })
}
