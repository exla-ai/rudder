//! Retained OFF-App census for the original machine's termination request.
//! This is deliberately not a machine drain receipt: covered joins cannot
//! settle ingress, Root, runtime services or backing aliases on their behalf.
use super::*;
use std::sync::OnceLock;

struct Request {
    ingress: crate::managed_machine_ingress::ManagedIngressControl,
    original: FinalizationBudget,
    cancelled: Arc<AtomicBool>,
}

struct Owners {
    covered: Option<CoveredTaskGenerationDrain>,
    error: Option<anyhow::Error>,
    panic: Option<Box<dyn Any + Send>>,
}

/// No successful constructor exists until complete production custody is
/// enrolled. Neither a covered census nor a successful worker join is enough.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MachineFinalRelease { Pending, Uncertain }

/// Owned outside Shared, including by the original Root worker's failure
/// retention loop. Shared must never store this slot: its retained ingress and
/// covered roots point back to Shared. No Drop joins or releases anything.
pub(super) struct RetainedMachineDrain {
    request: OnceLock<Request>,
    owners: Mutex<Owners>,
}

impl RetainedMachineDrain {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            request: OnceLock::new(),
            owners: Mutex::new(Owners { covered: None, error: None, panic: None }),
        })
    }

    /// Original App loop only. Capture once before fallible off-App closure.
    /// Repeated termination ticks never replace ingress, roots or the clock.
    pub(super) fn request_on_app(&self, control: &MachineControl,
        ingress: crate::managed_machine_ingress::ManagedIngressControl) -> Result<()> {
        ensure!(control.shared.app_thread == std::thread::current().id(),
            "machine termination request requires original App thread");
        ensure!(ingress.belongs_to_machine(control), "foreign termination ingress");
        if self.request.get().is_some() { return Ok(()); }
        self.request.set(Request {
            ingress, original: FinalizationBudget::begin(Duration::from_secs(30))?,
            cancelled: Arc::new(AtomicBool::new(false)),
        }).map_err(|_| anyhow::anyhow!("original machine termination already captured"))
    }

    /// Called by the SAME original Root worker, outside its carrier lock. The
    /// App never borrows these owners and cannot block on their Root/FS work.
    /// Every partial census/error/panic remains here, even after expiry. A
    /// failed campaign is not silently retried with a new cleanup budget.
    pub(super) fn poll_off_app(&self, control: &MachineControl) {
        let Some(request) = self.request.get() else { return; };
        let mut owners = self.owners.lock().unwrap_or_else(|p| p.into_inner());
        if owners.panic.is_some() { return; }
        let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            if owners.covered.is_none() {
                control.begin_covered_task_generation_drain_retaining(&mut owners.covered,
                    request.ingress.clone(), request.original.clone(), request.cancelled.clone())?;
            } else {
                control.poll_covered_task_generation_drain_retaining(owners.covered.as_mut().unwrap())?;
            }
            // Storage-subset integration belongs HERE, off App, under the
            // original carrier lock and request.original/request.cancelled:
            // bootstrap.retire_storage_service_retaining(...), which first
            // polls RuntimeImageClient::poll_unshared_retirement_retaining.
            // Retain and poll those same owners on false/error. Do not call it
            // until the full original census permits retiring that subset;
            // today's covered sample cannot establish that precondition.
            // Even an empty covered registry and every covered factory joined
            // leave other_runtime_producers Pending. Keep the original worker,
            // Apps, supervisors and engine owners alive; do not final-shutdown
            // an App or call EngineOwnership::finish from this census.
            Ok(())
        }));
        match outcome {
            Ok(Ok(())) => {},
            Ok(Err(error)) => {
                if owners.error.is_none() {
                    owners.error = Some(error);
                    eprintln!("MANAGED_MACHINE_DRAIN_UNCERTAIN");
                }
                // Keep polling the same retained campaign, including after
                // errors. Never clear uncertainty or renew its original clock.
            },
            Err(panic) => {
                owners.panic = Some(panic);
                eprintln!("MANAGED_MACHINE_DRAIN_UNCERTAIN");
            },
        }
    }

    /// Final App/Engine retirement seam, called only AFTER ManagedAppDrain
    /// reports true. It intentionally has no Ready variant in this slice.
    /// Integration must enroll a complete original producer census, retained
    /// Root stop ACK and SDK retirement owners, and physical joins of the
    /// original Root/ingress/drain workers before adding a successful branch
    /// that calls App::shutdown_with_reason and EngineOwnership::finish.
    /// Keep those owners in this carrier across every fallible final step.
    /// An off-App bootstrap.storage_service_retired() observation plugs in
    /// here as one required subset, never as a replacement for the complete
    /// census or original joins. No Root/client calls may move onto App.
    pub(super) fn final_release_on_app(&self, control: &MachineControl,
        original_root_worker: &OwnedWorkerJoin, app: &App,
        owner: &mut EngineOwnership) -> Result<MachineFinalRelease> {
        ensure!(control.shared.app_thread == std::thread::current().id(),
            "machine final release requires original App thread");
        let (repository, _) = owner.app_identity();
        ensure!(app.managed_project_mode && app.cwd == repository,
            "machine final release differs from actual managed App");
        owner.assert_app_cleanup_epoch()?;
        if self.request.get().is_none() { return Ok(MachineFinalRelease::Pending); }
        let owners = match self.owners.try_lock() {
            Ok(owners) => owners,
            Err(TryLockError::WouldBlock) => return Ok(MachineFinalRelease::Pending),
            Err(_) => return Ok(MachineFinalRelease::Uncertain),
        };
        if owners.error.is_some() || owners.panic.is_some() {
            return Ok(MachineFinalRelease::Uncertain);
        }
        if original_root_worker.try_joined()?.is_none() {
            return Ok(MachineFinalRelease::Pending);
        }
        let Some(covered) = &owners.covered else { return Ok(MachineFinalRelease::Pending); };
        match covered.progress_data().other_runtime_producers {
            UncoveredRuntimeProducers::Pending => Ok(MachineFinalRelease::Pending),
        }
    }
}
