//! Explicit Linux managed-local acceptance; blocking methods run OFF App.
//! Keep this owner outside catch_unwind and borrow it from the worker. Errors
//! retain its actual completion, lease, repository lock and conservative pins.
//! No Drop cleanup, publication retry, checkout migration or default dispatch.
#![cfg(target_os = "linux")]

use crate::{engine_host::EngineSourceLease, managed_launch::{PreparedManagedLaunch, WorkspaceAssignment},
    managed_store::ManagedRunLocation, workspace_vcs::WorkspaceVcsContext,
    managed_app::JoinedManagedTask};
use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use jj_lib::{backend::CommitId, config::{ConfigLayer, ConfigSource, StackedConfig},
    git_backend::GitBackend, object_id::ObjectId, repo::{Repo, RepoLoader, StoreFactories},
    settings::UserSettings};
use rudder_engine::{execution::{handoff::ValidatedManagedShutdown, CheckpointRef, ExecutionRecord},
    journal::{IntegrationEvidence, IntegrationReceipt, Journal, OwnerFence, TopicVersions,
        PreparedVcsOperation, VcsIntentReceipt, VcsIntentRequest, VcsIntentState,
        VcsPublicationObservation, VcsPublisher, VcsRunFence},
    protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef},
    snapshot::{self, RunnableRoot, SourceV2}, store::LocalStore, vfs::VfsCheckpoint,
    workspace::WorkspaceProfile};
use rudder_native::pty_terminal::{ManagedHelperCustody, SuccessfulManagedHelperExit, TerminalPane};
use rudder_vcs_bridge::{AttemptBinding, CommitIdentity, GitCommitId, JjOperationId, SealedSourceProof,
    journal_adapter::{persist_prepared_record, JournalJjPublisher},
    projection::{project_source, verify_versioned_source, VerifiedSourceProjection},
    projection_io::ProjectionBudget, publication::prepare_verified_operation};
use sha2::{Digest, Sha256};
use std::{fs::{self, File, OpenOptions}, os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf, time::{Duration, SystemTime, UNIX_EPOCH}};

/// Capture while the original pane is LIVE; strings/PIDs cannot reconstruct it.
pub(crate) struct ManagedCheckpointCapture {
    assignment: WorkspaceAssignment, custody: ManagedHelperCustody, helper_pid: u32,
    request_digest: String, operation: String, view: String, profile: String,
}
pub(crate) struct SettledManagedCheckpoint {
    capture: ManagedCheckpointCapture, shutdown: ValidatedManagedShutdown,
    _exit: SuccessfulManagedHelperExit, prepared: PreparedManagedLaunch,
}
/// The task variant keeps the original view, actors, helper and supervisor
/// shutdown hold together. Acceptance never extracts a copied completion from it.
enum ManagedCompletion {
    Pane(SettledManagedCheckpoint),
    Task(JoinedManagedTask),
}
impl ManagedCompletion {
    fn prepared(&self) -> &PreparedManagedLaunch {
        match self { Self::Pane(value) => &value.prepared, Self::Task(value) => value.prepared() }
    }
    fn assignment(&self) -> &WorkspaceAssignment { &self.prepared().assignment }
    fn shutdown(&self) -> &ValidatedManagedShutdown {
        match self { Self::Pane(value) => &value.shutdown, Self::Task(value) => value.shutdown() }
    }
}
impl ManagedCheckpointCapture {
    /// App-only, pure metadata/custody capture. No Journal/CAS/history IO.
    pub(crate) fn capture(prepared: &PreparedManagedLaunch, pane: &TerminalPane) -> Result<Self> {
        let spec = prepared.execution_spec()?;
        prepared.assignment.validate(&prepared.assignment.fence)?;
        ensure!(spec.format_version == 3 && spec.profile.is_none()
            && spec.execution_id == prepared.assignment.fence.attempt_id
            && spec.owner == prepared.assignment.fence.owner_epoch()
            && pane.instance_id() == spec.execution_id
            && pane.execution_profile() == Some(prepared.execution_profile.as_str())
            && matches!(prepared.execution_profile.as_str(), "codex-app-server" | "claude-sdk"),
            "acceptance requires the original managed-local model helper");
        Ok(Self { assignment: prepared.assignment.clone(), custody: pane.retain_managed_helper_custody()?,
            helper_pid: pane.child_process_id().context("helper is not live")?,
            request_digest: format!("{:x}", Sha256::digest(canonical_json(spec)?)),
            operation: spec.operation_id.clone(), view: spec.view_id.clone(), profile: prepared.execution_profile.clone() })
    }
    /// App-only. All fallible checks precede moving the original owning slots.
    /// Keep PreparedManagedLaunch in an Option in the caller's retained carrier.
    pub(crate) fn take_settled_retaining(capture: &mut Option<Self>,
        shutdown: &mut Option<ValidatedManagedShutdown>, prepared: &mut Option<PreparedManagedLaunch>,
        pane: &mut TerminalPane, expected_stop: &rudder_engine::execution::StopAndCheckpoint) -> Result<Option<SettledManagedCheckpoint>> {
        let held = capture.as_ref().context("acceptance capture absent")?;
        let ack = shutdown.as_ref().context("actual shutdown ACK absent")?;
        let launch = prepared.as_ref().context("original launch owner absent")?;
        ensure!(launch.assignment == held.assignment && pane.matches_managed_helper_custody(&held.custody)
            && pane.execution_profile() == Some(held.profile.as_str()) && ack.request() == expected_stop,
            "managed pane/launch/stop was replaced");
        validate_completion(held, ack.record(), ack.request())?;
        let Some(exit) = pane.successful_managed_helper_exit()? else { return Ok(None); };
        prepared.as_mut().unwrap().confirm_transient_settlement();
        Ok(Some(SettledManagedCheckpoint { capture: capture.take().unwrap(), shutdown: shutdown.take().unwrap(),
            _exit: exit, prepared: prepared.take().unwrap() }))
    }
}
fn validate_completion(capture: &ManagedCheckpointCapture, record: &ExecutionRecord,
    stop: &rudder_engine::execution::StopAndCheckpoint) -> Result<()> {
    validate_completion_identity(&capture.assignment.fence, &capture.operation, &capture.view,
        &capture.request_digest, capture.helper_pid, record, stop)
}
fn validate_completion_identity(fence: &crate::managed_launch::ManagedLaunchFence, operation: &str,
    view: &str, request_digest: &str, helper_pid: u32, record: &ExecutionRecord,
    stop: &rudder_engine::execution::StopAndCheckpoint) -> Result<()> {
    record.checkpointed_data()?;
    ensure!(record.format_version() == 1 && record.execution_id() == fence.attempt_id
        && record.operation_id() == operation && record.view_id() == view
        && record.owner() == &fence.owner_epoch() && record.request_digest() == request_digest
        && record.supervisor_pid() == helper_pid
        && stop.execution_id == fence.attempt_id && stop.operation_id == operation
        && stop.view_id == view && stop.run_id == fence.run_id
        && stop.run_epoch == fence.run_epoch && stop.owner == fence.owner_epoch()
        && matches!(record.checkpoint(), Some(CheckpointRef::CompleteLocal(_))),
        "shutdown/checkpoint differs from original managed helper");
    Ok(())
}

/// DATA for the canonical gate runner. Existence is neither gate execution nor acceptance.
#[derive(Clone, Debug)]
pub(crate) struct ManagedCheckpointCandidate {
    pub source: ObjectRef, pub environment: ObjectRef, pub runnable: ObjectRef,
    pub directory: ObjectRef, pub hardlinks: ObjectRef, pub profile: ObjectRef,
}
pub(crate) enum ManagedAcceptanceProgress {
    Prepared { intent_id: String },
    Published(IntegrationReceipt),
    RecoveryRequired(VcsIntentReceipt),
}
pub(crate) struct ManagedCheckpointAcceptance {
    completion: ManagedCompletion, lease: EngineSourceLease, location: ManagedRunLocation,
    store: LocalStore, repository: Option<PrivateIntegration>, candidate: Option<ManagedCheckpointCandidate>,
    binding: Option<AttemptBinding>, verified: Option<VerifiedSourceProjection>,
    projected: Option<jj_lib::merged_tree::MergedTree>, stages: Vec<String>,
    request: Option<VcsIntentRequest>, intent: Option<VcsIntentReceipt>, publish_attempted: bool,
    candidate_attempted: bool, preparation_attempted: bool,
}
impl ManagedCheckpointAcceptance {
    /// Off-App transfer. Failed CAS opening leaves both original owning slots.
    /// Root retains this owner through panic/stale rows and actual worker join.
    pub(crate) fn new_retaining(slot: &mut Option<Self>, completion: &mut Option<SettledManagedCheckpoint>,
        lease: &mut Option<EngineSourceLease>, location: ManagedRunLocation) -> Result<()> {
        ensure!(slot.is_none(), "acceptance owner already exists");
        let finished = completion.as_ref().context("actual completion absent")?;
        let held = lease.as_ref().context("live engine lease absent")?;
        ensure!(held.owner() == &finished.capture.assignment.fence.owner
            && finished.prepared.managed_run == location, "acceptance owner/managed run differs");
        held.check_current()?;
        let store = LocalStore::open(&finished.prepared.execution_spec()?.store_root)?;
        held.check_current()?;
        *slot = Some(Self { completion: ManagedCompletion::Pane(completion.take().unwrap()), lease: lease.take().unwrap(), location,
            store, repository: None, candidate: None, binding: None, verified: None, projected: None, stages: Vec::new(),
            request: None, intent: None, publish_attempted: false, candidate_attempted: false, preparation_attempted: false });
        Ok(())
    }
    /// OFF App. Every fallible check precedes moving either original owner.
    /// The complete joined capsule retains the supervisor hold even on panic,
    /// candidate failure, unknown publication or stale project ownership.
    pub(crate) fn new_joined_retaining(slot: &mut Option<Self>, completion: &mut Option<JoinedManagedTask>,
        lease: &mut Option<EngineSourceLease>, location: ManagedRunLocation) -> Result<()> {
        ensure!(slot.is_none(), "acceptance owner already exists");
        let finished = completion.as_ref().context("actual joined task absent")?;
        ensure!(!finished.is_settled(), "joined task was already discarded");
        let held = lease.as_ref().context("live engine lease absent")?;
        let prepared = finished.prepared();
        let assignment = &prepared.assignment;
        assignment.validate(&assignment.fence)?;
        ensure!(held.owner() == &assignment.fence.owner && prepared.managed_run == location,
            "acceptance owner/managed task differs");
        let spec = prepared.execution_spec()?;
        ensure!(spec.format_version == 3 && spec.profile.is_none()
            && spec.execution_id == assignment.fence.attempt_id
            && spec.owner == assignment.fence.owner_epoch()
            && matches!(prepared.execution_profile.as_str(), "codex-app-server" | "claude-sdk"),
            "acceptance requires the original managed model task");
        finished.helper_exit().matches_checkpoint(finished.helper_custody(), finished.shutdown())?;
        validate_completion_identity(&assignment.fence, &spec.operation_id, &spec.view_id,
            &format!("{:x}", Sha256::digest(canonical_json(spec)?)),
            finished.shutdown().record().supervisor_pid(), finished.shutdown().record(), finished.shutdown().request())?;
        held.check_current()?;
        let store = LocalStore::open(&spec.store_root)?;
        held.check_current()?;
        *slot = Some(Self { completion: ManagedCompletion::Task(completion.take().unwrap()),
            lease: lease.take().unwrap(), location, store, repository: None, candidate: None, binding: None,
            verified: None, projected: None, stages: Vec::new(), request: None, intent: None,
            publish_attempted: false, candidate_attempted: false, preparation_attempted: false });
        Ok(())
    }
    fn check(&self, journal: &Journal) -> Result<()> {
        self.lease.check_journal(journal)?;
        if let Some(repository) = &self.repository { repository.check_current()?; }
        let assignment = self.completion.assignment();
        assignment.fence.check_current(journal)?;
        ensure!(self.location.store().binding().project_id == assignment.fence.owner.project_id
            && self.location.assignment(&assignment.fence.attempt_id)? == Some(assignment.clone())
            && journal.managed_project(&assignment.fence.owner.project_id)?.as_ref() == Some(self.location.store().receipt()),
            "acceptance differs from durable prepared assignment");
        Ok(())
    }
    pub(crate) fn candidate(&self) -> Option<&ManagedCheckpointCandidate> { self.candidate.as_ref() }
    /// Full-local/source-only first slice. No live capture, partial overlay,
    /// ignored/prepared partition derivation or inherited environment mutation.
    pub(crate) fn prepare_candidate(&mut self, journal: &Journal, budget: ProjectionBudget) -> Result<()> {
        self.check(journal)?;
        ensure!(!self.candidate_attempted, "candidate preparation requires explicit recovery after any attempt");
        self.candidate_attempted = true;
        let assignment = self.completion.assignment();
        let context: WorkspaceVcsContext = serde_json::from_slice(&assignment.host_context.read_bytes()?)?;
        let binding = context.service.view.attempt;
        binding.validate()?;
        ensure!(binding.project_id == assignment.fence.owner.project_id && binding.owner == assignment.fence.owner_epoch()
            && binding.run_id == assignment.fence.run_id && binding.run_epoch == assignment.fence.run_epoch
            && binding.attempt_id == assignment.fence.attempt_id && binding.view_id == assignment.observation.view_id
            && binding.seed == assignment.seed && binding.profile == assignment.observation.profile
            && context.service.view.cas_root.canonicalize()? == self.store.root().canonicalize()?, "protected VCS attempt differs");
        self.repository = Some(PrivateIntegration::open(self.location.store().binding().directory.join("integration"),
            &context.service.user_name, &context.service.user_email)?);
        let stage = self.store.begin_staging(Duration::from_secs(120))?; self.stages.push(stage.id.clone());
        let Some(CheckpointRef::CompleteLocal(checkpoint)) = self.completion.shutdown().record().checkpoint()
            else { anyhow::bail!("only actual complete-local checkpoint is supported") };
        let base = snapshot::decode_runnable(&self.store.read_object(&assignment.runnable_root)?)?;
        ensure!(base.source == assignment.source, "base runnable/source assignment differs");
        let candidate = versioned_candidate(&self.store, &stage.id, &base, checkpoint, &binding.projection_policy)?;
        self.store.pin(&format!("managed-accept:{}:candidate", assignment.fence.request_id), &candidate.runnable)?;
        self.candidate = Some(candidate);
        self.binding = Some(binding);
        let repository = self.repository.as_ref().unwrap();
        let candidate = self.candidate.as_ref().unwrap();
        let heads = pollster::block_on(repository.loader.op_heads_store().get_op_heads())?;
        ensure!(heads.len() == 1, "private integration heads diverged");
        let operation = pollster::block_on(repository.loader.load_operation(&heads[0]))?;
        let repo = pollster::block_on(repository.loader.load_at(&operation))?;
        let base_commit = repo.store().get_commit(&CommitId::new(decode_hex(&self.binding.as_ref().unwrap().baseline.commit.0)?))?;
        ensure!(CommitIdentity::from_jj(&base_commit) == self.binding.as_ref().unwrap().baseline, "integration baseline differs");
        let projected = pollster::block_on(project_source(self.store.clone(), &candidate.directory,
            &base_commit.tree(), &self.binding.as_ref().unwrap().projection_policy, None, None, budget.clone()))?;
        self.projected = Some(projected.tree);
        self.verified = Some(pollster::block_on(verify_versioned_source(self.store.clone(), &stage.id,
            &candidate.source, &self.binding.as_ref().unwrap().projection_policy,
            self.projected.as_ref().unwrap(), Some(&projected.record), budget))?);
        self.check(journal)
    }
    /// Caller supplies ACTUAL canonical gate evidence for candidate(), never a
    /// done-file or backend turn report. This module creates only mapping evidence.
    pub(crate) fn prepare_acceptance(&mut self, journal: &mut Journal,
        mut evidence: IntegrationEvidence, versions: TopicVersions) -> Result<ManagedAcceptanceProgress> {
        self.check(journal)?;
        ensure!(!self.preparation_attempted, "acceptance preparation already attempted; recover original intent");
        let candidate = self.candidate.as_ref().context("candidate absent")?;
        let binding = self.binding.as_ref().context("attempt binding absent")?;
        let verified = self.verified.as_ref().context("actual jj mapping verification absent")?;
        validate_gate_input(candidate, binding, &evidence)?;
        self.preparation_attempted = true;
        if !evidence.action_results.contains(verified.evidence()) { evidence.action_results.push(verified.evidence().clone()); }
        self.request = Some(VcsIntentRequest { idempotency_key: format!("managed-accept:{}", self.completion.assignment().fence.request_id),
            source_run: VcsRunFence { run_id: binding.run_id.clone(), run_epoch: binding.run_epoch,
                attempt_id: binding.attempt_id.clone(), view_id: binding.view_id.clone() }, expected_head: binding.baseline.commit.0.clone(),
            proposed_source_root: candidate.source.clone(), proposed_environment_root: candidate.environment.clone(),
            recovery_root: candidate.runnable.clone(), seed_root: binding.seed.clone(), projection_policy: binding.projection_policy.clone(),
            profile: candidate.profile.clone(), expected_view_root: Some(candidate.directory.clone()), hardlinks: Some(candidate.hardlinks.clone()), versions, evidence });
        self.intent = Some(journal.begin_vcs_intent(self.lease.owner(), &self.store, self.request.as_ref().unwrap(), now_ms()?)?);
        if self.intent.as_ref().unwrap().state != VcsIntentState::Accepted { return self.progress(); }
        let repository = self.repository.as_ref().unwrap();
        let heads = pollster::block_on(repository.loader.op_heads_store().get_op_heads())?;
        ensure!(heads.len() == 1, "integration head set changed");
        let operation = pollster::block_on(repository.loader.load_operation(&heads[0]))?;
        let repo = pollster::block_on(repository.loader.load_at(&operation))?;
        let mut tx = repo.start_transaction();
        let commit = pollster::block_on(tx.repo_mut().new_commit(
            vec![CommitId::new(decode_hex(&binding.baseline.commit.0)?)], self.projected.as_ref().unwrap().clone())
            .set_description("managed checkpoint acceptance").write())?;
        let proof = SealedSourceProof { format_version: 1, binding: binding.clone(), recovery_root: candidate.runnable.clone(),
            source: candidate.source.clone(), environment: Some(candidate.environment.clone()), tree: CommitIdentity::from_jj(&commit).tree };
        let prepared = pollster::block_on(prepare_verified_operation(tx, self.intent.as_ref().unwrap().intent_id.clone(),
            heads.iter().map(|id| JjOperationId(id.hex())).collect(), GitCommitId(binding.baseline.commit.0.clone()),
            CommitIdentity::from_jj(&commit), proof, verified))?;
        let stage = self.store.begin_staging(Duration::from_secs(120))?; self.stages.push(stage.id.clone());
        let envelope = persist_prepared_record(&self.store, &stage.id, &prepared)?;
        repository.check_current()?;
        self.intent = Some(journal.attach_vcs_operation(self.lease.owner(), &self.store,
            &self.intent.as_ref().unwrap().intent_id, &envelope, now_ms()?)?);
        repository.check_current()?;
        self.progress()
    }
    pub(crate) fn publish_once(&mut self, journal: &mut Journal) -> Result<ManagedAcceptanceProgress> {
        self.check(journal)?;
        ensure!(!self.publish_attempted && self.intent.as_ref().is_some_and(|r| r.state == VcsIntentState::Prepared),
            "publication cannot replay; inspect original intent");
        self.publish_attempted = true;
        let mut publisher = CheckedJjPublisher { repository: self.repository.as_ref().unwrap(), store: &self.store };
        self.intent = Some(journal.publish_vcs_intent(self.lease.owner(), &self.intent.as_ref().unwrap().intent_id,
            &mut publisher, now_ms()?)?);
        self.repository.as_ref().unwrap().check_current()?;
        self.progress()
    }
    /// Readback/reconciliation only: never prepares or republishes an operation.
    pub(crate) fn reconcile_only(&mut self, journal: &mut Journal) -> Result<ManagedAcceptanceProgress> {
        self.check(journal)?;
        if self.intent.is_none() {
            self.intent = Some(journal.begin_vcs_intent(self.lease.owner(), &self.store,
                self.request.as_ref().context("no original immutable intent to reconcile")?, now_ms()?)?);
        }
        let id = self.intent.as_ref().unwrap().intent_id.clone();
        self.intent = Some(journal.vcs_intent(self.lease.owner(), &id)?);
        if matches!(self.intent.as_ref().unwrap().state, VcsIntentState::Started | VcsIntentState::Uncertain) {
            let mut publisher = CheckedJjPublisher { repository: self.repository.as_ref().unwrap(), store: &self.store };
            self.intent = Some(journal.reconcile_vcs_intent(self.lease.owner(), &id, &mut publisher, now_ms()?)?);
        }
        self.repository.as_ref().context("private integration owner absent")?.check_current()?;
        self.progress()
    }
    /// Release the original task's view/cache retention and supervisor hold only
    /// after this owner has observed durable publication of its exact intent.
    /// An unknown publication, rejected intent or cleanup error keeps the same
    /// capsule. Candidate/recovery CAS pins are intentionally retained here.
    pub(crate) fn settle_published_task(&mut self, journal: &Journal,
        budget: &rudder_engine::execution::finalization_budget::FinalizationBudget) -> Result<IntegrationReceipt> {
        budget.remaining()?;
        self.check(journal)?;
        let receipt = self.intent.as_ref().context("original publication acknowledgement absent")?;
        ensure!(receipt.state == VcsIntentState::Published, "task acceptance is not published");
        let current = journal.vcs_intent(self.lease.owner(), &receipt.intent_id)?;
        ensure!(current == *receipt, "published task acceptance changed");
        let integration = receipt.integration.clone().context("published task acceptance receipt absent")?;
        let ManagedCompletion::Task(completion) = &mut self.completion
            else { anyhow::bail!("acceptance does not own a joined task"); };
        budget.remaining()?;
        completion.settle_discard_off_app(budget)?;
        Ok(integration)
    }
    /// Pure borrowing access for the native seed-retirement bridge. The WHOLE
    /// acceptor remains owned by that bridge; this never extracts/reconstructs a
    /// completion, promotes an unknown publication, or creates an ACK.
    pub(crate) fn settled_joined_task(&self)->Result<&JoinedManagedTask> {
        let receipt=self.intent.as_ref().context("original publication ACK absent")?;
        ensure!(receipt.state==VcsIntentState::Published&&receipt.integration.is_some(),"original acceptance is not Published");
        let ManagedCompletion::Task(joined)=&self.completion else {anyhow::bail!("original joined task absent");};
        ensure!(joined.is_settled(),"Published task cleanup is not settled");
        joined.helper_exit().matches_checkpoint(joined.helper_custody(),joined.shutdown())?;
        Ok(joined)
    }
    fn progress(&self) -> Result<ManagedAcceptanceProgress> {
        let receipt = self.intent.as_ref().context("intent ACK absent")?;
        if receipt.state == VcsIntentState::Published {
            Ok(ManagedAcceptanceProgress::Published(receipt.integration.clone().context("publication has no integration receipt")?))
        } else if receipt.state == VcsIntentState::Prepared && !self.publish_attempted {
            Ok(ManagedAcceptanceProgress::Prepared { intent_id: receipt.intent_id.clone() })
        } else { Ok(ManagedAcceptanceProgress::RecoveryRequired(receipt.clone())) }
    }
}

/// OFF App after restart. Only the NEW actual Fabric owner authorizes readback;
/// old receipts are DATA and cannot recreate the old completion or publish again.
pub(crate) fn reconcile_managed_acceptance(journal: &mut Journal, lease: &EngineSourceLease,
    store: &crate::managed_store::ManagedStore, cas: &LocalStore,
    intent_id: &str, name: &str, email: &str) -> Result<ManagedAcceptanceProgress> {
    lease.check_journal(journal)?;
    ensure!(lease.owner().project_id == store.binding().project_id
        && journal.managed_project(&lease.owner().project_id)?.as_ref() == Some(store.receipt()), "recovery project differs");
    let repository = PrivateIntegration::open(store.binding().directory.join("integration"), name, email)?;
    // Caller retains the explicitly configured project/shared CAS. Immutable
    // hashes and the existing bridge validate every original record read.
    let receipt = journal.vcs_intent(lease.owner(), intent_id)?;
    let mut publisher = CheckedJjPublisher { repository: &repository, store: cas };
    let receipt = if matches!(receipt.state, VcsIntentState::Started | VcsIntentState::Uncertain) {
        journal.reconcile_vcs_intent(lease.owner(), intent_id, &mut publisher, now_ms()?)?
    } else { receipt };
    repository.check_current()?;
    if receipt.state == VcsIntentState::Published {
        Ok(ManagedAcceptanceProgress::Published(receipt.integration.context("published receipt lacks integration")?))
    } else { Ok(ManagedAcceptanceProgress::RecoveryRequired(receipt)) }
}
fn validate_gate_input(candidate: &ManagedCheckpointCandidate, binding: &AttemptBinding, evidence: &IntegrationEvidence) -> Result<()> {
    ensure!(evidence.tested_head == binding.baseline.commit.0 && evidence.tested_source_root == candidate.source
        && evidence.tested_environment_root == candidate.environment, "canonical gates tested another candidate/base");
    Ok(())
}

fn versioned_candidate(store: &LocalStore, stage: &str, base: &RunnableRoot,
    checkpoint: &VfsCheckpoint, policy: &ObjectRef) -> Result<ManagedCheckpointCandidate> {
    let profile: WorkspaceProfile = serde_json::from_slice(&store.read_object(&base.profile)?)?;
    profile.validate()?;
    let source = snapshot::decode_source(&store.read_object(&base.source)?)?;
    ensure!(profile.prepared.is_empty() && profile.scratch.is_empty() && profile.vcs_local.is_empty()
        && base.view_root == source.directory && source.projection_policy == *policy
        && checkpoint.format_version == 1, "first acceptance profile requires complete source-only namespace");
    let environment = base.environment.clone().context("acceptance needs an explicit retained environment identity")?;
    snapshot::decode_environment(&store.read_object(&environment)?)?;
    let source = snapshot::stage(store, stage, ObjectKind::SourceRoot, &SourceV2 { format_version: 2,
        consistency: CheckpointConsistency::Quiesced, directory: checkpoint.directory.clone(), projection_policy: policy.clone() })?;
    let runnable = snapshot::stage(store, stage, ObjectKind::RunnableRoot, &RunnableRoot { format_version: 1,
        consistency: CheckpointConsistency::Quiesced, source: source.clone(), environment: Some(environment.clone()),
        view_root: checkpoint.directory.clone(), profile: base.profile.clone(), hardlinks: checkpoint.hardlinks.clone(),
        knowledge_cursor: base.knowledge_cursor })?;
    Ok(ManagedCheckpointCandidate { source, environment, runnable, directory: checkpoint.directory.clone(),
        hardlinks: checkpoint.hardlinks.clone(), profile: base.profile.clone() })
}
struct IntegrationDirectory { path: PathBuf, held: File }
impl IntegrationDirectory {
    fn open(path: PathBuf) -> Result<Self> {
        // Canonical equality also rejects symlinked ancestors. This is the
        // fixed pinned-jj internal layout, not an external/colocated Git store.
        ensure!(path.canonicalize()? == path, "symlinked integration hierarchy is unsupported");
        let held = OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NONBLOCK).open(&path)?;
        let value = Self { path, held }; value.check_current()?; Ok(value)
    }
    fn check_current(&self) -> Result<()> {
        let held = self.held.metadata()?; let current = fs::symlink_metadata(&self.path)?;
        ensure!(self.path.canonicalize()? == self.path && held.is_dir() && current.is_dir()
            && held.uid() == unsafe { libc::geteuid() } && current.uid() == held.uid()
            && held.mode() & 0o022 == 0 && current.mode() & 0o022 == 0
            && (held.dev(), held.ino()) == (current.dev(), current.ino()),
            "private integration directory custody changed");
        Ok(())
    }
}
struct PrivateIntegration {
    loader: RepoLoader, directories: Vec<IntegrationDirectory>, directory: File,
    // The actual exclusive lock drops after loader and retained directory IO.
    lock: File, path: PathBuf,
}
impl PrivateIntegration {
    fn open(path: PathBuf, name: &str, email: &str) -> Result<Self> {
        ensure!(path.is_absolute() && path.canonicalize()? == path, "integration checkout must already exist at its exact private path");
        let meta = fs::symlink_metadata(&path)?;
        ensure!(meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o777 == 0o700,
            "integration checkout must be private and owned");
        let directory = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_DIRECTORY).open(&path)?;
        let lock = OpenOptions::new().read(true).write(true).create(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK).open(path.join("rudder-acceptance.lock"))?;
        let meta = lock.metadata()?;
        ensure!(meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o777 == 0o600, "invalid integration lock");
        lock.try_lock_exclusive().context("private integration is already owned")?;
        // RepoLoader retains pathname-based stores. Anchor each stable mutable
        // store root and its hierarchy before constructing it, not just /integration.
        let mut directories = Vec::new();
        for relative in [".jj", ".jj/repo", ".jj/repo/store", ".jj/repo/op_store", ".jj/repo/op_heads",
            ".jj/repo/index", ".jj/repo/submodule_store", ".jj/repo/store/git", ".jj/repo/store/git/objects"] {
            directories.push(IntegrationDirectory::open(path.join(relative))?);
        }
        let loader = RepoLoader::init_from_file_system(&settings(name, email)?, &path.join(".jj/repo"), &StoreFactories::default())?;
        let git = loader.store().backend_impl::<GitBackend>().context("integration requires pinned Git backend")?;
        let expected_git = path.join(".jj/repo/store/git");
        ensure!(git.git_repo_path() == expected_git.as_path()
            && expected_git.canonicalize()? == expected_git, "integration requires its exact canonical internal Git store");
        let held = Self { loader, directories, lock, directory, path }; held.check_current()?; Ok(held)
    }
    fn check_current(&self) -> Result<()> {
        for (held, path) in [(&self.directory, self.path.clone()), (&self.lock, self.path.join("rudder-acceptance.lock"))] {
            let actual = held.metadata()?; let current = fs::symlink_metadata(path)?;
            ensure!((actual.dev(), actual.ino()) == (current.dev(), current.ino()), "private integration custody changed");
        }
        for directory in &self.directories { directory.check_current()?; }
        Ok(())
    }
}
/// Run the same retained-path checks at the actual Journal callback boundary,
/// and again after IO. A replacement during IO becomes uncertainty, never a
/// successful publication/reconciliation claim or a second publish attempt.
struct CheckedJjPublisher<'a> { repository: &'a PrivateIntegration, store: &'a LocalStore }
impl VcsPublisher for CheckedJjPublisher<'_> {
    fn inspect(&mut self, prepared: &PreparedVcsOperation) -> Result<VcsPublicationObservation> {
        self.repository.check_current()?;
        let result = JournalJjPublisher { loader: &self.repository.loader, store: self.store }.inspect(prepared);
        self.repository.check_current()?; result
    }
    fn publish(&mut self, prepared: &PreparedVcsOperation) -> Result<VcsPublicationObservation> {
        self.repository.check_current()?;
        let result = JournalJjPublisher { loader: &self.repository.loader, store: self.store }.publish(prepared);
        self.repository.check_current()?; result
    }
}
fn settings(name: &str, email: &str) -> Result<UserSettings> {
    ensure!([name, email].iter().all(|v| !v.is_empty() && v.len() <= 1024 && !v.chars().any(char::is_control)), "invalid integration author");
    let mut config = StackedConfig::with_defaults(); let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer.set_value("user.name", name)?; layer.set_value("user.email", email)?; config.add_layer(layer);
    Ok(UserSettings::from_config(config)?)
}
fn decode_hex(value: &str) -> Result<Vec<u8>> {
    ensure!(value.len() % 2 == 0 && value.len() <= 128 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "invalid raw identity");
    Ok(value.as_bytes().chunks_exact(2).map(|pair| {
        let digit = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        digit(pair[0]) * 16 + digit(pair[1])
    }).collect())
}
fn now_ms() -> Result<i64> { Ok(i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?) }

#[cfg(test)]
#[path = "managed_checkpoint_accept_regressions.rs"]
mod tests;
