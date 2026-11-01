//! Child of managed_checkpoint_accept: only a whole genuine Published/settled
//! acceptor can mint this original input. Saved receipt/path/HEAD data cannot.
use super::*;
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use rudder_vcs_bridge::{initial_source::PreparedInitialSource,publication::PreparedJjOperation};
use std::sync::atomic::{AtomicBool,Ordering};

pub(crate) struct AcceptedSourceInput {
    identity:Arc<()>,owner:OwnerFence,receipt:VcsIntentReceipt,repo:Arc<jj_lib::repo::ReadonlyRepo>,
    verified:Arc<VerifiedSourceProjection>,directories:Vec<IntegrationDirectory>,
}
impl AcceptedSourceInput {
    pub(crate) fn same_origin(&self,identity:&Arc<()>)->bool{Arc::ptr_eq(&self.identity,identity)}
    pub(crate) fn owner(&self)->&OwnerFence{&self.owner}
    pub(crate) fn receipt(&self)->&VcsIntentReceipt{&self.receipt}
    pub(crate) fn integration(&self)->&IntegrationReceipt{self.receipt.integration.as_ref().unwrap()}
    pub(crate) fn repository(&self)->&Arc<jj_lib::repo::ReadonlyRepo>{&self.repo}
    pub(crate) fn verified(&self)->&Arc<VerifiedSourceProjection>{&self.verified}
    pub(crate) fn check_physical(&self)->Result<()> {for directory in &self.directories{directory.check_current()?;}Ok(())}
    pub(crate) fn check_current(&self,lease:&EngineSourceLease,journal:&Journal,budget:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(budget,cancel)?;lease.wait_shared_current(budget,cancel)?;lease.check_journal(journal)?;self.check_physical()?;
        let state=journal.project_state(&self.owner.project_id)?;
        ensure!(lease.owner()==&self.owner&&state.owner_active&&state.owner==self.owner
            &&state.integration_head==self.integration().new_head
            &&journal.vcs_intent(&self.owner,&self.receipt.intent_id)?==self.receipt,
            "accepted source input no longer matches actual owner/Published head");time(budget,cancel)
    }
}
/// Original canonical-lock cleanup ACK only. It grants no task/source use;
/// the native coordinator separately retains genuine factory/TLS proof.
pub(crate) struct AcceptedInputRelease {identity:Arc<()>,_lock:File,acknowledged:bool}
impl AcceptedInputRelease {pub(crate) fn acknowledged(&self)->bool{self.acknowledged}pub(crate) fn same_input(&self,input:&AcceptedSourceInput)->bool{Arc::ptr_eq(&self.identity,&input.identity)}}
impl ManagedCheckpointAcceptance {
    /// OFF App. Stage a writer from this WHOLE original acceptor's actual held
    /// lease, before fallible postchecks. The canonical gate worker retains this
    /// Option alongside the acceptor outside catch/unwind and through its join.
    /// No supplied path/owner DATA or cleanup-only fenced lease can open it.
    pub(crate) fn capture_acceptance_writer_retaining(&self,out:&mut Option<Journal>,
        budget:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        ensure!(out.is_none(),"original acceptance writer already captured");time(budget,cancel)?;
        self.lease.wait_shared_current(budget,cancel)?;
        *out=Some(self.lease.open_source_writer()?);
        self.check(out.as_ref().unwrap())?;self.lease.wait_shared_current(budget,cancel)?;time(budget,cancel)
    }
    /// OFF App, borrowed and effect-free except original FD aliases. The whole
    /// acceptor remains owned by its old task/seed scope through actual retirement.
    pub(crate) fn capture_source_successor_input(&mut self,journal:&Journal,previous:&PreparedInitialSource,
        budget:&FinalizationBudget,cancel:&AtomicBool)->Result<Arc<AcceptedSourceInput>> {
        time(budget,cancel)?;self.lease.wait_shared_current(budget,cancel)?;self.check(journal)?;
        self.settled_joined_task()?;
        let receipt=self.intent.as_ref().context("original Published receipt absent")?;
        let integration=receipt.integration.as_ref().context("original integration receipt absent")?;
        let prepared=receipt.prepared.as_ref().context("original prepared jj operation absent")?;
        let assignment=self.completion.assignment();
        ensure!(receipt.state==VcsIntentState::Published&&receipt.original_owner==*self.lease.owner()
            &&receipt.project_id==self.lease.owner().project_id&&integration.project_id==receipt.project_id
            &&integration.owner_epoch==self.lease.owner().epoch&&integration.previous_head==previous.baseline.commit.0
            &&integration.new_head==prepared.commit_id&&receipt.request.expected_head==integration.previous_head
            &&assignment.runnable_root==previous.runnable_root&&assignment.source==previous.source_root&&assignment.seed==previous.seed_root,
            "Published successor differs from the actual previous source/seed/task");
        ensure!(journal.vcs_intent(self.lease.owner(),&receipt.intent_id)?==*receipt
            &&journal.project_state(&receipt.project_id)?.integration_head==integration.new_head,
            "Published source is not the current authoritative Journal head");
        let repository=self.repository.as_ref().context("original integration owner absent")?;
        repository.check_current()?;
        let mut publisher=CheckedJjPublisher{repository,store:&self.store};
        ensure!(publisher.inspect(prepared)?==VcsPublicationObservation::Published,
            "actual published jj operation/head/bookmark changed");
        let bytes=self.store.read_object(&prepared.record)?;
        let record:PreparedJjOperation=serde_json::from_slice(&bytes)?;
        ensure!(canonical_json(&record)?==bytes,"published jj record is not canonical");
        let verified=self.verified.as_ref().context("original actual mapping verifier absent")?;
        verified.check_proof(&record.proof)?;
        ensure!(integration.proposed_source_root==record.proof.source
            &&record.proof.environment.as_ref()==Some(&integration.proposed_environment_root)
            &&record.proof.source==receipt.request.proposed_source_root,
            "Published source/environment differs from original verified operation");
        let op=pollster::block_on(repository.loader.load_operation(&jj_lib::op_store::OperationId::new(decode_hex(&prepared.operation_id)?)))?;
        let repo=pollster::block_on(repository.loader.load_at(&op))?;
        let commit=repo.store().get_commit(&CommitId::new(decode_hex(&prepared.commit_id)?))?;
        ensure!(CommitIdentity::from_jj(&commit)==record.commit
            &&repo.view().get_wc_commit_id(jj_lib::ref_name::WorkspaceName::DEFAULT)==Some(commit.id()),
            "Published operation lacks exact accepted WC/commit identity");
        let identity=self.generation_identity.get_or_insert_with(||Arc::new(())).clone();
        let mut directories=Vec::with_capacity(repository.directories.len()+1);
        directories.push(IntegrationDirectory{path:repository.path.clone(),held:repository.directory.try_clone()?});
        for d in &repository.directories{directories.push(IntegrationDirectory{path:d.path.clone(),held:d.held.try_clone()?});}
        let input=Arc::new(AcceptedSourceInput{identity,owner:self.lease.owner().clone(),receipt:receipt.clone(),repo,verified:verified.clone(),directories});
        input.check_current(&self.lease,journal,budget,cancel)?;repository.check_current()?;Ok(input)
    }
    /// Explicit OFF-App lock release only after the original successor source
    /// has a genuine pinned ACK. Preserve the whole old completion and receipts.
    /// An Err retains the original file/flag; never infer an unlock from absence.
    pub(crate) fn release_generation_lock_retaining(&mut self,out:&mut Option<AcceptedInputRelease>,source:&crate::managed_initial_source::ManagedInitialSourceOwner,
        journal:&Journal,budget:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(budget,cancel)?;self.lease.wait_shared_current(budget,cancel)?;
        source.check_successor_origin(self.generation_identity.as_ref().context("original successor identity absent")?,self.intent.as_ref().context("Published source absent")?,budget,cancel)?;
        self.check(journal)?;let repository=self.repository.as_mut().context("original integration owner absent")?;
        ensure!(out.is_none(),"original canonical input release already attempted");repository.check_current()?;
        *out=Some(AcceptedInputRelease{identity:self.generation_identity.as_ref().unwrap().clone(),_lock:repository.lock.try_clone()?,acknowledged:false});
        FileExt::unlock(&repository.lock)?;out.as_mut().unwrap().acknowledged=true;repository.lock_held=false;
        time(budget,cancel)
    }
}
fn time(budget:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {if cancel.load(Ordering::Acquire){budget.cancel();anyhow::bail!("original accepted source job cancelled");}budget.remaining()?;Ok(())}
