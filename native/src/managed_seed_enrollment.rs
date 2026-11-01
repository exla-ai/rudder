//! Trusted task-to-Root enrollment only. No scheduler, worker spawning, default
//! dispatch, saved-ACK adoption, Root-owner reconstruction or automatic cleanup.
#![cfg(target_os = "linux")]
use crate::{App, managed_app::{PendingManagedTaskHelper, JoinedManagedTask},
    managed_launch::{ManagedLaunchFence, WorkspaceAssignment},
    managed_store::{ManagedRunLocation, ManagedStore}};
use anyhow::{ensure, Context, Result};
use rudder_engine::{execution::finalization_budget::FinalizationBudget,
    journal::{Journal, OwnerFence}, protocol::ObjectRef};
use rudder_native::root_storage_client::{ClientSeedOwner, EnrolledSeedHelper, RootStorageClient};
use std::{any::Any, panic::{catch_unwind, AssertUnwindSafe}, sync::atomic::{AtomicBool, Ordering}};

#[cfg(test)]
#[path = "managed_seed_enrollment_tests.rs"]
mod tests;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase { Pending, Enrolling, AckRetained, Verified, Releasing, Released, Recovery }

/// Stage in bounded machine-owned storage BEFORE calling enroll_retaining.
/// Keep this SAME owner outside callbacks/catches; a failed channel delivery
/// must not drop it. Original RootStorageClient + ClientSeedOwner remain held
/// separately by the machine, across every callback/error and all task drains.
/// No Clone/Serde/public receipt constructor and no Drop retirement/unpin.
#[must_use = "retain actual pending helper and Root ACK through whole task retirement"]
pub(crate) struct ManagedSeedEnrollment {
    pending: PendingManagedTaskHelper,
    receipt: Option<EnrolledSeedHelper>,
    phase: Phase,
    request_attempted: bool,
    release_attempted: bool,
    panic: Option<Box<dyn Any + Send>>,
}
impl ManagedSeedEnrollment {
    /// Pure transfer of the actual App-produced pending helper/slot ownership.
    pub(crate) fn new(pending: PendingManagedTaskHelper) -> Self {
        Self { pending, receipt: None, phase: Phase::Pending, request_attempted: false,
            release_attempted: false, panic: None }
    }
    pub(crate) fn fence(&self) -> &ManagedLaunchFence { self.pending.fence() }
    pub(crate) fn assignment(&self) -> &WorkspaceAssignment { self.pending.assignment() }
    pub(crate) fn receipt(&self) -> Option<&EnrolledSeedHelper> { self.receipt.as_ref() }
    /// Observations only: neither false nor an error proves helper/writer drain.
    pub(crate) fn request_attempted(&self) -> bool { self.request_attempted }
    pub(crate) fn release_attempted(&self) -> bool { self.release_attempted }
    pub(crate) fn released(&self) -> bool { self.phase == Phase::Released }
    pub(crate) fn requires_recovery(&self) -> bool { self.phase == Phase::Recovery }

    /// OFF App, one attempt only. The actual mutable client/seed stay OUTSIDE
    /// this adapter and inner unwind boundary; their retained uncertain state
    /// must not be replaced, retried from paths, or promoted by readback.
    pub(crate) fn enroll_retaining(&mut self, client: &mut RootStorageClient,
        seed: &mut ClientSeedOwner) -> Result<()> {
        ensure!(self.phase == Phase::Pending, "managed seed enrollment is one-shot; original owners retained");
        self.phase = Phase::Enrolling;
        let result = catch_unwind(AssertUnwindSafe(|| self.enroll_inner(client, seed)));
        self.retain_outcome(result, "managed seed enrollment panicked; original owners retained")
    }
    fn enroll_inner(&mut self, client: &mut RootStorageClient, seed: &mut ClientSeedOwner) -> Result<()> {
        check_time(self.pending.original_budget(), self.pending.cancellation())?;
        let expected_seed = seed.seed().clone();
        self.check_current_assignment(&expected_seed)?;
        let pidfd = self.pending.helper().duplicate_live_pidfd_for_seed_enrollment()?;
        check_time(self.pending.original_budget(), self.pending.cancellation())?;
        // Absorbing BEFORE entering the client API: an Err/panic/late ACK may
        // already have enrolled a real Root reader. Never repeat this request.
        self.request_attempted = true;
        let receipt = client.enroll_original_helper_retaining(seed, pidfd, self.pending.original_budget())?;
        self.receipt = Some(receipt); // BEFORE every fallible post-ACK check
        self.phase = Phase::AckRetained;
        check_time(self.pending.original_budget(), self.pending.cancellation())?;
        ensure!(!client.enrolled_helper_exited(seed, self.receipt.as_ref().unwrap())?,
            "enrolled original helper exited before start release");
        check_time(self.pending.original_budget(), self.pending.cancellation())?;
        // Recheck the SAME actual witness/cutoff. This duplicate is process DATA
        // and never promotes pending custody to readiness/dispatch authority.
        let _original = self.pending.helper().duplicate_live_pidfd_for_seed_enrollment()?;
        check_time(self.pending.original_budget(), self.pending.cancellation())?;
        self.check_current_assignment(seed.seed())?;
        check_time(self.pending.original_budget(), self.pending.cancellation())?;
        self.phase = Phase::Verified;
        Ok(())
    }
    fn check_current_assignment(&self, seed: &ObjectRef) -> Result<()> {
        let host = self.pending.journal_owner();
        host.with_writer(self.pending.original_budget(), self.pending.cancellation(), |journal| {
            current_assignment(journal, host.owner(), self.pending.managed_run(), self.pending.assignment(),
                self.pending.fence(), seed, self.pending.original_budget(), self.pending.cancellation())
        })
    }
    /// App-only pure post-ACK adoption. All IO/fresh Journal/location checks ran
    /// off App under the original budget. App separately checks its CURRENT
    /// row/pane/helper Arc/slot generation and original clock before release.
    /// Borrowed adoption preserves the SAME pending owner even during unwind.
    /// Keep this adapter + receipt through future whole Joined task retirement.
    pub(crate) fn release_on_app_retaining(&mut self, app: &mut App) -> Result<()> {
        ensure!(self.phase == Phase::Verified && self.receipt.is_some(),
            "genuine verified Root enrollment ACK required before App release");
        self.phase = Phase::Releasing;
        let result = catch_unwind(AssertUnwindSafe(|| {
            check_time(self.pending.original_budget(), self.pending.cancellation())?;
            self.release_attempted = true;
            app.release_managed_task_start_after_enrollment_retaining(&self.pending)
        }));
        match self.retain_outcome(result, "managed start release panicked; dispatch may have occurred; owners retained") {
            Ok(()) => { self.phase = Phase::Released; Ok(()) }
            Err(error) => Err(error),
        }
    }
    /// Additional matching only, never retirement permission. The original
    /// Joined capsule, actual Root client/seed/helper-exit checks, publication
    /// and explicit Root retirement ACK remain separate retained obligations.
    pub(crate) fn matches_joined(&self, joined: &JoinedManagedTask) -> Result<()> {
        ensure!(self.phase == Phase::Released && self.receipt.is_some(), "managed seed start is not known released");
        ensure!(joined.prepared().assignment == *self.pending.assignment(), "joined task differs from enrolled assignment");
        joined.helper_exit().matches_checkpoint(joined.helper_custody(), joined.shutdown())
    }
    fn retain_outcome(&mut self, outcome: std::thread::Result<Result<()>>, panic_message: &'static str) -> Result<()> {
        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => { self.phase = Phase::Recovery; Err(error) }
            Err(panic) => { self.panic = Some(panic); self.phase = Phase::Recovery; Err(anyhow::anyhow!(panic_message)) }
        }
    }
}

fn check_time(original: &FinalizationBudget, cancelled: &AtomicBool) -> Result<()> {
    ensure!(!cancelled.load(Ordering::Acquire), "managed seed enrollment cancelled");
    original.remaining()?; Ok(())
}

/// Existing typed location only. A saved path/JSON never creates an engine,
/// source lease, managed floor or Root seed owner. Checks are observations at
/// their actual off-App execution point, not an atomic fence against later
/// external mutation; App adoption performs its separate current-row check.
fn current_assignment(journal: &Journal, owner: &OwnerFence, location: &ManagedRunLocation,
    assignment: &WorkspaceAssignment, fence: &ManagedLaunchFence, seed: &ObjectRef,
    original: &FinalizationBudget, cancelled: &AtomicBool) -> Result<()> {
    check_time(original, cancelled)?;
    assignment.validate(fence)?;
    ensure!(owner == &fence.owner && assignment.seed == *seed
        && location.run_id() == fence.run_id && location.store().binding().project_id == owner.project_id,
        "Root seed enrollment differs from the original task/owner/seed");
    fence.check_current(journal)?; check_time(original, cancelled)?;
    ensure!(ManagedStore::open(journal, location.store().binding())? == *location.store(),
        "managed seed enrollment floor differs from held Journal");
    check_time(original, cancelled)?;
    let saved = location.assignment(&fence.attempt_id)?.context("original managed assignment missing")?;
    ensure!(saved == *assignment, "managed seed enrollment assignment changed");
    check_time(original, cancelled)?;
    fence.check_current(journal)?; check_time(original, cancelled)
}
