//! Receipt data stays provisional until Native joins its owned helper wait and
//! an actual host issuer query. No Serde/Clone or public receipt constructor.
use super::*;
use crate::vfs::partial_rpc::PartialBinding;

pub struct ProvisionalSourceShutdown {
    receipt: ManagedShutdownReceipt,
    binding: PartialBinding,
    helper_pid: u32,
    budget: FinalizationBudget,
}

impl ProvisionalSourceShutdown {
    /// Call only inside the authenticated FD receiver, after its pending-stop,
    /// backend-exit and drain checks. Artifact admission validates Record2 and
    /// its exact source checkpoint family; it grants no parent process proof.
    pub(super) fn received(
        binding: &ManagedExecutionBinding,
        digest: &str,
        helper_pid: u32,
        receipt: ManagedShutdownReceipt,
        budget: FinalizationBudget,
    ) -> Result<Self> {
        budget.remaining()?;
        ensure!(binding.format_version == 4, "source join requires binding4");
        receipt.request.validate(binding)?;
        validate_shutdown_artifact(binding, digest, &receipt)?;
        let partial = binding
            .profile
            .as_ref()
            .context("source profile absent")?
            .partial_binding()?
            .context("source join requires remote source")?;
        ensure!(
            helper_pid > 1
                && receipt.record.supervisor_pid() == helper_pid
                && receipt.kernel_drained
                && receipt.record.agent_exit().is_some()
                && receipt.record.phase() == ExecutionPhase::Checkpointed
                && receipt.record.request_digest() == digest,
            "source receipt differs from its actual receiver"
        );
        budget.remaining()?;
        Ok(Self {
            receipt,
            binding: partial,
            helper_pid,
            budget,
        })
    }

    /// Identity comparison only. A PID supplied here is never process proof;
    /// Native must separately consume its privately constructed child witness.
    pub fn matches_helper(&self, attempt: &str, pid: u32) -> bool {
        self.binding.attempt.execution_id == attempt && self.helper_pid == pid
    }
    pub fn binding(&self) -> &PartialBinding {
        &self.binding
    }
    pub fn budget(&self) -> &FinalizationBudget {
        &self.budget
    }
    pub fn request(&self) -> &StopAndCheckpoint {
        &self.receipt.request
    }
    pub fn record(&self) -> &ExecutionRecord {
        &self.receipt.record
    }
}
