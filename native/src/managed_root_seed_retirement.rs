//! Binary-private original task -> shared seed-use join bridge. OFF App only.
//! No issuer from bool/JSON/paths/type erasure; preserve the actual completion,
//! original enrolled helper ACK and publication owner through Root retirement.
#![cfg(target_os="linux")]
use crate::{managed_app::JoinedManagedTask,managed_checkpoint_accept::ManagedCheckpointAcceptance,
    managed_seed_enrollment::ManagedSeedEnrollment};
use anyhow::{ensure,Context,Result};
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use rudder_native::{root_storage_client::{RootStorageClient,ClientSeedOwner},
    root_storage_uses::{SeedUseGuard,RetainedSeedJoin,JoinedSeedUse}};

enum Completion {Discarded(JoinedManagedTask),Published(ManagedCheckpointAcceptance)}
impl Completion {
    fn joined(&self)->Result<&JoinedManagedTask> {
        match self {Self::Discarded(value)=>{ensure!(value.is_settled(),"explicit discard not settled");Ok(value)},
            Self::Published(value)=>value.settled_joined_task()}
    }
}
struct OriginalTaskOwners {completion:Completion,enrollment:ManagedSeedEnrollment}
fn confirm_original_use(owners:&mut OriginalTaskOwners)->Result<()> {
    // These existing opaque carriers contain actual helper/control/runtime/
    // broker/PTY-reader joins. This new worker's join does not replace them.
    owners.enrollment.matches_joined(owners.completion.joined()?)
}

/// Keep in bounded machine storage outside catches/results. The shared ledger
/// also retains the actual joined state/T even if this typed result is dropped.
/// RootStorageClient + ClientSeedOwner stay separately machine-owned. No Drop
/// cleanup/unmount and no live App/TerminalPane may enter this carrier.
#[must_use="retain original joined task/acceptance/enrollment through Root retirement ACK"]
pub(crate) struct ManagedSeedTaskDrain {
    pending:Option<OriginalTaskOwners>,worker:Option<RetainedSeedJoin<OriginalTaskOwners>>,
    joined:Option<JoinedSeedUse<OriginalTaskOwners>>,original:FinalizationBudget,
}
impl ManagedSeedTaskDrain {
    /// No start, wait, retry or finalization. Empty pre-transfer slots are not
    /// completion: only this SAME existing cleanup's actual cached join may
    /// expose the original whole discarded/published owner and enrollment.
    pub(crate) fn observe_original_runtime_use(&self)
        ->Result<Option<crate::managed_app::runtime_task::OriginalTaskRuntimeUse>> {
        let observed=if self.joined.is_none() {
            if let Some(worker)=&self.worker {worker.reconcile_joined()?} else {None}
        } else {None};
        let Some(joined)=self.joined.as_ref().or(observed.as_ref()) else {return Ok(None);};
        joined.with_value(|owners|owners.enrollment.observe_joined_runtime_use(owners.completion.joined()?)).map(Some)
    }
    fn validate(client:&RootStorageClient,seed:&ClientSeedOwner,guard:&Option<SeedUseGuard>,
        joined:&JoinedManagedTask,enrollment:&ManagedSeedEnrollment,original:&FinalizationBudget)->Result<()> {
        original.remaining()?;ensure!(joined.is_settled(),"task acceptance/discard cleanup not settled");
        enrollment.matches_joined(joined)?;
        ensure!(&joined.prepared().assignment.seed==seed.seed(),"task uses another original seed");
        client.associate_original_helper_use(seed,guard.as_ref().context("pre-admitted task seed guard absent")?,
            enrollment.receipt().context("genuine original helper enrollment ACK absent")?)?;
        original.remaining()?;Ok(())
    }
    /// Original owning slots survive every Err. Stage this output owner BEFORE
    /// moving the genuine capsules; a late stage failure leaves them in pending.
    pub(crate) fn stage_discarded_retaining(out:&mut Option<Self>,guard:&mut Option<SeedUseGuard>,
        joined:&mut Option<JoinedManagedTask>,enrollment:&mut Option<ManagedSeedEnrollment>,
        client:&RootStorageClient,seed:&ClientSeedOwner,original:&FinalizationBudget)->Result<()> {
        ensure!(out.is_none(),"task seed drain already staged");
        Self::validate(client,seed,guard,joined.as_ref().context("original joined task absent")?,
            enrollment.as_ref().context("original enrollment absent")?,original)?;
        *out=Some(Self {pending:None,worker:None,joined:None,original:original.clone()});
        let owner=out.as_mut().unwrap();
        owner.pending=Some(OriginalTaskOwners {completion:Completion::Discarded(joined.take().unwrap()),enrollment:enrollment.take().unwrap()});
        owner.finish_staging_retaining(guard)
    }
    /// Retains WHOLE Published acceptor, including original lease/checkpoint/
    /// publication receipts and candidate pins; no extraction of copied proofs.
    pub(crate) fn stage_published_retaining(out:&mut Option<Self>,guard:&mut Option<SeedUseGuard>,
        acceptance:&mut Option<ManagedCheckpointAcceptance>,enrollment:&mut Option<ManagedSeedEnrollment>,
        client:&RootStorageClient,seed:&ClientSeedOwner,original:&FinalizationBudget)->Result<()> {
        ensure!(out.is_none(),"task seed drain already staged");
        Self::validate(client,seed,guard,acceptance.as_ref().context("original acceptance absent")?.settled_joined_task()?,
            enrollment.as_ref().context("original enrollment absent")?,original)?;
        *out=Some(Self {pending:None,worker:None,joined:None,original:original.clone()});
        let owner=out.as_mut().unwrap();
        owner.pending=Some(OriginalTaskOwners {completion:Completion::Published(acceptance.take().unwrap()),enrollment:enrollment.take().unwrap()});
        owner.finish_staging_retaining(guard)
    }
    pub(crate) fn finish_staging_retaining(&mut self,guard:&mut Option<SeedUseGuard>)->Result<()> {
        RetainedSeedJoin::stage_retaining(&mut self.worker,guard,&mut self.pending,&self.original)
    }
    pub(crate) fn start_off_app(&mut self)->Result<()> {
        self.worker.as_mut().context("original task seed join not staged")?.start_retaining(confirm_original_use)
    }
    pub(crate) fn await_joined_off_app(&mut self)->Result<()> {
        self.joined=Some(self.worker.as_mut().context("original task seed join absent")?.await_joined()?);Ok(())
    }
    pub(crate) fn poll_joined_off_app(&mut self)->Result<bool> {
        if self.joined.is_none() {self.joined=self.worker.as_ref().context("original task join absent")?.reconcile_joined()?;}
        Ok(self.joined.is_some())
    }
    /// Retry SAME actual carrier + original budget only. Unknown publication is
    /// never admitted above, and this callback performs no publication/IPC.
    pub(crate) fn retry_failed_off_app(&mut self)->Result<()> {
        self.worker.as_mut().context("original task seed join absent")?.retry_failed_retaining(confirm_original_use)
    }
}
