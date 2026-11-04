//! Native startup projection. The parent supplies the existing engine journal
//! module so the same adapter is exercised by the engine's crash test harness.
//! No recovery disposition grants permission to spawn or resume a backend.
use super::recovery_journal::{Journal, OwnerFence, Recovery, RuntimePhase};
use anyhow::{ensure, Result};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ExpectedRuntimeAttempt {
    pub run_id: String,
    pub run_epoch: u64,
    pub attempt_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) enum RuntimeQuarantineReason {
    MissingObservation,
    ReplacedAttempt,
    UncertainCustody,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) enum RecoveredRuntimeDisposition {
    Paused(ExpectedRuntimeAttempt),
    Completed(ExpectedRuntimeAttempt),
    Failed(ExpectedRuntimeAttempt),
    Quarantined {
        attempt: ExpectedRuntimeAttempt,
        reason: RuntimeQuarantineReason,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
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
        "native runtime recovery exceeds 4096 attempts"
    );
    let effects = journal.recover_runtime_restart(owner, now_ms)?;
    // Historical terminal observations can legitimately precede the current
    // owner epoch. They remain recovery evidence, although the live discovery
    // API correctly hides them from scheduling and runtime control.
    let observations=journal.recorded_runtime_inventory(owner)?.into_iter()
        .map(|runtime|(runtime.run_id.clone(),runtime)).collect::<std::collections::BTreeMap<_,_>>();
    let mut runtimes = Vec::with_capacity(expected.len());
    for attempt in expected {
        let runtime = observations.get(&attempt.run_id);
        let disposition = match runtime {
            None => RecoveredRuntimeDisposition::Quarantined {
                attempt: attempt.clone(),
                reason: RuntimeQuarantineReason::MissingObservation,
            },
            Some(runtime)
                if runtime.run_epoch != attempt.run_epoch
                    || runtime.attempt_id != attempt.attempt_id =>
            {
                RecoveredRuntimeDisposition::Quarantined {
                    attempt: attempt.clone(),
                    reason: RuntimeQuarantineReason::ReplacedAttempt,
                }
            }
            Some(runtime) if !runtime.pty_owned => match runtime.phase {
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
