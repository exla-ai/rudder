//! Private Spec5 preparation, deliberately absent from App/default dispatch.
//! Call on a bounded worker with an actual lease and privileged current Journal.
//! Records/paths/PIDs cannot construct the lease, peer or joined shutdown proof.
use super::ManagedLaunchFence;
use crate::{
    agent_broker::{self, AgentBroker, BrokerConfig, BrokerThreadOwner, BrokerThreadsSettled},
    engine_host::EngineSourceLease,
    managed_store::{ManagedRunLocation, ManagedStore},
    remote_workspace::{self, PreparedContextRuntime, RemoteContextInputs},
    source_authorizer::{
        HostSourceAuthority, HostSourceRequestPeer, Limits, SourceAuthorizerServer,
        SourceAuthorizerSettled,
    },
    source_issuer::HostSourceIssuer,
    source_shutdown_join::JoinedSourceShutdown,
};
use anyhow::{ensure, Context, Result};
use rudder_engine::{
    agent_api::AgentIdentityFile,
    execution::{
        finalization_budget::FinalizationBudget, handoff::helper_process::HelperProcessWitness,
        startup_budget::{StartupConstraint, STARTUP_POLICY_LIMIT},
        network::NetworkProfileV1, BrokerBinding, ExecutionProfileV4, ExecutionSpec,
        ExecutionWorkspaceSelectionV4, ManagedExecutionBinding, ManagedFrontendStart,
        ProtectedHostContextRef, ReadOnlyRuntime, WorkspaceObservationProfile,
    },
    journal::{
        host_source_scope, CapabilityGrant, HeldJournalFile, Journal, PreparedCapabilityIssuance,
        HOST_SOURCE_REQUIRED_OPERATION,
    },
    protocol::canonical_json,
    vfs::{
        partial_rpc::{partial_scope, PartialBinding, REQUIRED_OPERATION},
        remote_launch::RemoteLaunchRequest,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PREPARATION_LIMIT: Duration = Duration::from_secs(20);
const ACTIVATION_LIMIT: Duration = Duration::from_secs(10);
const STARTUP_LIMIT: Duration = STARTUP_POLICY_LIMIT;
const FINALIZATION_SECONDS: u64 = 30;
const GRANT_MARGIN_SECONDS: u64 = 5;

/// Original wall expiry is fixed before any preparation IO. The guest grant
/// excludes the host-only finalization allowance; neither is ever renewed.
struct SourceGrantBounds {
    start_ms: i64,
    execution_ms: i64,
    host_expiry_ms: i64,
    guest_expiry_ms: i64,
}
impl SourceGrantBounds {
    fn new(start_ms: i64, timeout_seconds: u64) -> Result<Self> {
        ensure!(
            start_ms >= 0 && (1..=86400).contains(&timeout_seconds),
            "invalid source time budget"
        );
        let expiry = |extra: u64| -> Result<i64> {
            let seconds = timeout_seconds
                .checked_add(STARTUP_LIMIT.as_secs())
                .and_then(|n| n.checked_add(extra))
                .context("source lifetime overflow")?;
            let milliseconds = seconds
                .checked_mul(1000)
                .context("source lifetime overflow")?;
            start_ms
                .checked_add(i64::try_from(milliseconds)?)
                .context("source expiry overflow")
        };
        Ok(Self {
            start_ms,
            execution_ms: i64::try_from(
                timeout_seconds
                    .checked_mul(1000)
                    .context("source execution lifetime overflow")?,
            )?,
            host_expiry_ms: expiry(FINALIZATION_SECONDS + GRANT_MARGIN_SECONDS)?,
            guest_expiry_ms: expiry(GRANT_MARGIN_SECONDS)?,
        })
    }
    fn activation_end(prepared: Duration) -> Result<Duration> {
        Ok(prepared
            .checked_add(ACTIVATION_LIMIT)
            .context("source activation overflow")?
            .min(STARTUP_LIMIT))
    }
    fn check_phase(
        &self,
        elapsed: Duration,
        wall_ms: i64,
        phase_end: Duration,
    ) -> Result<Duration> {
        let phase_ms = i64::try_from(phase_end.as_millis())?;
        let wall_elapsed = wall_ms
            .checked_sub(self.start_ms)
            .context("source clock overflow")?;
        ensure!(
            wall_elapsed >= 0 && elapsed < phase_end && wall_elapsed < phase_ms,
            "source startup deadline expired or clock regressed"
        );
        let host_needed = self
            .execution_ms
            .checked_add(i64::try_from(
                (FINALIZATION_SECONDS + GRANT_MARGIN_SECONDS) * 1000,
            )?)
            .context("source admission lifetime overflow")?;
        let guest_needed = self
            .execution_ms
            .checked_add(i64::try_from(GRANT_MARGIN_SECONDS * 1000)?)
            .context("guest admission lifetime overflow")?;
        ensure!(
            self.host_expiry_ms
                .checked_sub(wall_ms)
                .is_some_and(|remaining| remaining > host_needed)
                && self
                    .guest_expiry_ms
                    .checked_sub(wall_ms)
                    .is_some_and(|remaining| remaining > guest_needed),
            "source grant lacks execution and finalization lifetime"
        );
        Ok(
            (phase_end - elapsed).min(Duration::from_millis(u64::try_from(
                phase_ms - wall_elapsed,
            )?)),
        )
    }
}

/// Reuses the suspend-aware, absorbing thirty-second clock primitive only as a
/// time bound. No shutdown, execution or source authority is minted by it.
struct SourceStartupBudget {
    budget: FinalizationBudget,
    bounds: SourceGrantBounds,
    prepared_at: OnceLock<Duration>,
    last_wall_ms: Mutex<i64>,
}
impl SourceStartupBudget {
    fn begin(timeout_seconds: u64) -> Result<Arc<Self>> {
        let start = now_ms()?;
        Ok(Arc::new(Self {
            bounds: SourceGrantBounds::new(start, timeout_seconds)?,
            budget: FinalizationBudget::begin(STARTUP_LIMIT)?,
            prepared_at: OnceLock::new(),
            last_wall_ms: Mutex::new(start),
        }))
    }
    fn sample(&self, phase_end: Duration) -> Result<(Duration, Duration)> {
        let result = (|| {
            // Serialize complete samples so concurrent preparation/request
            // checks cannot mistake an older in-flight sample for regression.
            let mut last = self
                .last_wall_ms
                .lock()
                .map_err(|_| anyhow::anyhow!("source clock poisoned"))?;
            let elapsed = STARTUP_LIMIT
                .checked_sub(self.budget.remaining()?)
                .context("source startup clock changed")?;
            let wall = now_ms()?;
            ensure!(wall >= *last, "source wall clock regressed");
            *last = wall;
            let remaining = self.bounds.check_phase(elapsed, wall, phase_end)?;
            let wall_elapsed = Duration::from_millis(u64::try_from(wall - self.bounds.start_ms)?);
            Ok((elapsed.max(wall_elapsed), remaining))
        })();
        if result.is_err() {
            self.budget.cancel();
        }
        result
    }
    fn check_preparation(&self) -> Result<()> {
        self.sample(PREPARATION_LIMIT).map(|_| ())
    }
    fn finish_preparation(&self) -> Result<()> {
        let (elapsed, _) = self.sample(PREPARATION_LIMIT)?;
        self.prepared_at
            .set(elapsed)
            .map_err(|_| anyhow::anyhow!("source preparation already settled"))
    }
    fn activation_remaining(&self) -> Result<Duration> {
        let prepared = *self
            .prepared_at
            .get()
            .context("source preparation pending")?;
        let end = SourceGrantBounds::activation_end(prepared)?;
        self.sample(end).map(|(_, remaining)| remaining)
    }
}

/// All identities are App-reserved before this immutable request enters IO.
/// No provider/profile lookup, inherited environment or accepted-history claim.
pub(crate) struct ManagedSourcePrepareRequest {
    lease: EngineSourceLease,
    fence: ManagedLaunchFence,
    selection: RemoteLaunchRequest,
    inputs: ManagedSourceInputs,
}
pub(crate) struct ManagedSourceInputs {
    pub managed_store: ManagedStore,
    pub endpoint_parent: PathBuf,
    pub context: RemoteContextInputs,
    pub network: NetworkProfileV1,
    pub runtime: Vec<ReadOnlyRuntime>,
    pub command: Vec<String>,
    pub timeout_seconds: u64,
}
impl ManagedSourcePrepareRequest {
    pub(crate) fn fence(&self) -> &ManagedLaunchFence { &self.fence }
    pub(crate) fn selection(&self) -> &RemoteLaunchRequest { &self.selection }
    pub(crate) fn managed_run(&self) -> Result<ManagedRunLocation> {
        self.inputs.managed_store.run(&self.fence.run_id)
    }
    pub(crate) fn journal_path(&self) -> &Path {
        &self.inputs.managed_store.binding().journal
    }

    /// Pure typed construction. An actual lease is consumed; no saved owner or
    /// database path is accepted in its place. IO/issuance happens in prepare.
    pub(crate) fn new(
        lease: EngineSourceLease,
        fence: ManagedLaunchFence,
        selection: RemoteLaunchRequest,
        inputs: ManagedSourceInputs,
    ) -> Result<Self> {
        fence.validate()?;
        selection.validate()?;
        ensure!(
            lease.owner() == &fence.owner
                && selection.request_id == fence.request_id
                && selection.attempt.project_id == fence.owner.project_id
                && selection.attempt.owner == fence.owner_epoch()
                && selection.attempt.run_id == fence.run_id
                && selection.attempt.run_epoch == fence.run_epoch
                && selection.attempt.execution_id == fence.attempt_id,
            "remote source selection differs from its actual reserved owner/run"
        );
        Ok(Self {
            lease,
            fence,
            selection,
            inputs,
        })
    }
}

pub(crate) use super::RemoteSourceAssignment;

/// Pending ingress consumes an existing charged server worker, with a deadline
/// shorter than the client's two-second request budget. It performs no Journal
/// query or grant renewal. Only actual helper custody can fill this slot.
struct ActivationPeer(ActivationSlot<HelperProcessWitness>);
impl HostSourceRequestPeer for ActivationPeer {
    fn check_peer(&self, stream: &UnixStream) -> Result<()> {
        self.0.wait(Duration::from_secs(1))?.verify_peer(stream)
    }
}
enum ActivationState<T> {
    Pending,
    Ready(Arc<T>),
    Failed,
}
struct ActivationSlot<T> {
    state: Mutex<ActivationState<T>>,
    changed: Condvar,
    deadline: Instant,
    startup: Option<Arc<SourceStartupBudget>>,
}
impl<T> ActivationSlot<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(ActivationState::Pending),
            changed: Condvar::new(),
            deadline: Instant::now() + ACTIVATION_LIMIT,
            startup: None,
        }
    }
    fn with_startup(startup: Arc<SourceStartupBudget>) -> Result<Self> {
        let remaining = startup.budget.remaining()?;
        Ok(Self {
            deadline: Instant::now()
                .checked_add(remaining)
                .context("source activation deadline overflow")?,
            startup: Some(startup),
            ..Self::new()
        })
    }
    fn remaining(&self) -> Result<Duration> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .context("source activation expired")?;
        Ok(if let Some(startup) = &self.startup {
            let phase = if startup.prepared_at.get().is_some() {
                startup.activation_remaining()?
            } else {
                startup.sample(PREPARATION_LIMIT)?.1
            };
            remaining.min(phase)
        } else {
            remaining
        })
    }
    fn fail(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = ActivationState::Failed;
        }
        self.changed.notify_all();
    }
    fn install(&self, value: Arc<T>) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("source activation poisoned"))?;
        ensure!(
            matches!(*state, ActivationState::Pending),
            "source activation expired or already settled"
        );
        self.remaining()?;
        if let Some(startup) = &self.startup {
            startup.activation_remaining()?;
        }
        *state = ActivationState::Ready(value);
        self.changed.notify_all();
        Ok(())
    }
    fn check_pending(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("source activation poisoned"))?;
        if matches!(*state, ActivationState::Pending) && self.remaining().is_err() {
            *state = ActivationState::Failed;
            self.changed.notify_all();
        }
        ensure!(
            matches!(*state, ActivationState::Pending),
            "source activation already settled"
        );
        Ok(())
    }
    fn wait(&self, timeout: Duration) -> Result<Arc<T>> {
        self.wait_pending(timeout, || {})
    }
    fn wait_pending(&self, timeout: Duration, pending: impl FnOnce()) -> Result<Arc<T>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("source request wait overflow")?
            .min(self.deadline);
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("source activation poisoned"))?;
        let mut pending = Some(pending);
        loop {
            match &*state {
                ActivationState::Ready(value) => return Ok(value.clone()),
                ActivationState::Failed => anyhow::bail!("source activation failed"),
                ActivationState::Pending => {}
            }
            let now = Instant::now();
            let remaining = match self.remaining() {
                Ok(remaining) => remaining,
                Err(error) => {
                    *state = ActivationState::Failed;
                    return Err(error);
                }
            };
            ensure!(now < deadline, "source activation is pending or expired");
            if let Some(pending) = pending.take() {
                pending();
            }
            state = self
                .changed
                .wait_timeout(state, (deadline - now).min(remaining))
                .map_err(|_| anyhow::anyhow!("source activation poisoned"))?
                .0;
        }
    }
}
struct Secret(String);
impl Drop for Secret {
    fn drop(&mut self) {
        unsafe {
            self.0.as_bytes_mut().fill(0);
        }
    }
}

pub(crate) struct PreparedManagedSource {
    assignment: RemoteSourceAssignment,
    spec: ExecutionSpec,
    binding: ManagedExecutionBinding,
    pub managed_run: ManagedRunLocation,
    resources: Option<SourceResources>,
    adopted: bool,
}
struct SourceResources {
    journal: HeldJournalFile,
    journal_identity: (u64, u64),
    issuer: Arc<HostSourceIssuer>,
    server: SourceAuthorizerServer,
    guest: AgentBroker,
    guest_threads: BrokerThreadOwner,
    guest_settled: Option<BrokerThreadsSettled>,
    peer: Arc<ActivationPeer>,
    host_token: Secret,
    guest_token: Secret,
    host_issue: PreparedCapabilityIssuance,
    guest_issue: PreparedCapabilityIssuance,
    runtime: PreparedContextRuntime,
    context: HeldContext,
    source_paths: OwnedLeaves,
    context_paths: OwnedLeaves,
    settled: Option<SourceAuthorizerSettled>,
    host_revoked: bool,
}
impl PreparedManagedSource {
    pub(crate) fn assignment(&self) -> &RemoteSourceAssignment {
        &self.assignment
    }
    pub(crate) fn execution_spec(&self) -> &ExecutionSpec {
        &self.spec
    }
    pub(crate) fn binding(&self) -> &ManagedExecutionBinding {
        &self.binding
    }
    pub(crate) fn issuer(&self) -> Result<Arc<HostSourceIssuer>> {
        Ok(Arc::clone(
            &self
                .resources
                .as_ref()
                .context("source preparation retired")?
                .issuer,
        ))
    }
    /// App-safe retention only. App must first adopt this exact fence/selection
    /// and persist its managed projection; helper activation remains separate.
    pub(crate) fn adopt(&mut self) {
        self.adopted = true;
    }
    /// Off-App. The required adapter must retain the actual activated helper
    /// witness. This function cannot mint it from child ID, JSON or an FD number.
    pub(crate) fn activate_helper(&mut self, helper: Arc<HelperProcessWitness>) -> Result<()> {
        ensure!(self.adopted, "source preparation has not been adopted");
        let resources = self
            .resources
            .as_mut()
            .context("source preparation retired")?;
        let result = (|| {
            resources.peer.0.check_pending()?;
            helper.verify_binding(&self.binding)?;
            let check = || resources.peer.0.check_pending();
            resources.context.check_current_checked(&check)?;
            resources.runtime.check_current_checked(&check)?;
            resources.source_paths.check_current()?;
            resources.context_paths.check_current()?;
            resources.issuer.authorize(
                &self
                    .binding
                    .profile
                    .as_ref()
                    .context("source profile absent")?
                    .partial_binding()?
                    .context("source binding absent")?,
            )?;
            helper.verify_binding(&self.binding)?;
            resources.peer.0.install(helper)
        })();
        if result.is_err() {
            resources.peer.0.fail();
        }
        result
    }
    /// Off-App. Closing/revoking guest tools does not touch host source service,
    /// its capability, runtime descriptors or EngineSourceLease.
    pub(crate) fn stop_guest(&self, journal: &mut Journal) -> Result<()> {
        let resources = self
            .resources
            .as_ref()
            .context("source preparation retired")?;
        resources.journal.check_current()?;
        let supplied = journal.hold_database_file()?;
        let metadata = fs::symlink_metadata(journal.database_path()?)?;
        ensure!(
            (metadata.dev(), metadata.ino()) == resources.journal_identity,
            "source retirement Journal differs from the original owner connection"
        );
        supplied.check_current()?;
        resources.guest.close();
        journal.revoke_capability(
            &self.assignment.fence.owner,
            &resources.guest_token.0,
            now_ms()?,
        )
    }
    /// Off-App, after StopV4 was queued. This proves actual guest callback/
    /// acceptor/TLS settlement under the ORIGINAL finalization budget. The
    /// opaque proof stays inside this exact prepared owner for joined retirement.
    pub(crate) fn settle_guest(&mut self, journal: &mut Journal, budget: &FinalizationBudget) -> Result<()> {
        budget.remaining()?;
        self.stop_guest(journal)?;
        let timeout = budget.remaining()?.min(Duration::from_secs(2));
        let resources = self.resources.as_mut().context("source preparation retired")?;
        journal.retire_prepared_capability(&mut resources.guest_issue, now_ms()?)?;
        if resources.guest_settled.is_none() {
            resources.guest_settled = Some(resources.guest_threads.await_closed(timeout)?);
        }
        budget.remaining()?;
        Ok(())
    }
    pub(crate) fn retire_unused(&mut self, journal: &mut Journal) -> Result<()> {
        ensure!(!self.adopted, "adopted source requires joined finalization");
        self.retire(journal, None)
    }
    /// Off-App, after App joined the exact provisional packet, its actually
    /// reaped successful helper and fresh issuer query under the original budget.
    pub(crate) fn retire_joined(
        &mut self,
        joined: &JoinedSourceShutdown,
        journal: &mut Journal,
    ) -> Result<()> {
        let resources = self
            .resources
            .as_ref()
            .context("source preparation retired")?;
        ensure!(
            joined.matches_issuer(&resources.issuer)
                && joined.provisional()?.binding()
                    == &self
                        .binding
                        .profile
                        .as_ref()
                        .unwrap()
                        .partial_binding()?
                        .unwrap(),
            "joined source shutdown differs from this actual preparation"
        );
        self.retire(journal, Some(joined.provisional()?.budget()))
    }
    fn retire(&mut self, journal: &mut Journal, budget: Option<&FinalizationBudget>) -> Result<()> {
        let remaining = || -> Result<Duration> {
            match budget { Some(b) => Ok(b.remaining()?.min(Duration::from_secs(2))),
                None => Ok(Duration::from_secs(2)) }
        };
        remaining()?;
        self.stop_guest(journal)?;
        let resources = self
            .resources
            .as_mut()
            .context("source preparation retired")?;
        journal.retire_prepared_capability(&mut resources.guest_issue, now_ms()?)?;
        // Neither acceptor closure nor counters prove request/TLS settlement.
        if resources.guest_settled.is_none() {
            resources.guest_settled = Some(resources.guest_threads.await_closed(remaining()?)?);
        }
        // Never infer settlement from close, cancellation, timeout or a record.
        // A failed wait leaves this owner and its handles available for retry.
        if resources.settled.is_none() {
            resources.settled = Some(resources.server.await_closed(remaining()?)?);
        }
        if !resources.host_revoked {
            journal.retire_prepared_capability(&mut resources.host_issue, now_ms()?)?;
            resources.host_revoked = true;
        }
        remaining()?;
        resources.source_paths.remove()?;
        resources.context_paths.remove()?;
        remaining()?;
        self.resources.take();
        Ok(())
    }
}
impl Drop for PreparedManagedSource {
    fn drop(&mut self) {
        // App Drop performs no IO or waits and cannot shut down an uncertain
        // adopted helper's source. The caller MUST route unused/joined results
        // through its bounded worker. Abandoned ownership requires recovery.
        if let Some(resources) = self.resources.take() {
            std::mem::forget(resources);
        }
    }
}

fn now_ms() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
fn new_private(path: &Path) -> Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(path.parent().context("private file parent absent")?)?.sync_all()?;
    Ok(())
}
fn grant(
    fence: &ManagedLaunchFence,
    operation: &str,
    scope: String,
    expiry: i64,
) -> CapabilityGrant {
    CapabilityGrant {
        principal: format!("{operation}:{}", fence.attempt_id),
        run_id: fence.run_id.clone(),
        run_epoch: fence.run_epoch,
        operations: BTreeSet::from([operation.into()]),
        scopes: BTreeSet::from([scope]),
        recipients: BTreeMap::new(),
        topics: BTreeSet::new(),
        expires_at_ms: Some(expiry),
    }
}

/// No App access, PTY/provider calls, RemoteSession creation or accepted-history
/// mutation. The actual helper creates its session after the namespace change.
/// Typed ownership, deliberately NOT std::error::Error: converting a failed
/// owner into anyhow would discard its only explicit settlement/retry handle.
#[must_use = "retain this owner and its admission until is_settled"]
pub(crate) struct ManagedSourcePrepareFailure {
    _cause: anyhow::Error,
    resources: Option<PreparationResources>,
}
impl std::fmt::Debug for ManagedSourcePrepareFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedSourcePrepareFailure").field("diagnostic", &self.diagnostic())
            .field("settled", &self.is_settled()).finish()
    }
}
impl std::fmt::Display for ManagedSourcePrepareFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.diagnostic()) }
}
impl ManagedSourcePrepareFailure {
    pub(crate) fn diagnostic(&self) -> &'static str {
        match self.resources.as_ref() {
            None => "source preparation failed; no effects or explicitly settled",
            Some(r) if r.issuance_uncertain => "source capability issuance uncertain; retained recovery required",
            Some(_) => "source preparation failed; actual owned cleanup remains",
        }
    }
    pub(crate) fn is_settled(&self) -> bool { self.resources.is_none() }
    /// Off-App, retryable on this SAME value. Each wait bounds cleanup only;
    /// it never restarts startup/finalization clocks or grants source authority.
    pub(crate) fn settle_unused(&mut self, journal: &mut Journal) -> Result<()> {
        let Some(r) = self.resources.as_mut() else { return Ok(()) };
        r.settle_unused(journal)?;
        self.resources.take();
        Ok(())
    }
}

/// This carrier is allocated OUTSIDE construction/catch_unwind. Constructors
/// write live owners into its slots before any subsequent fallible operation.
/// Drop only requests retirement; it is never settlement/admission release.
struct PreparationResources {
    fence: ManagedLaunchFence,
    effects_started: bool,
    issuance_uncertain: bool,
    host_issue: Option<PreparedCapabilityIssuance>,
    guest_issue: Option<PreparedCapabilityIssuance>,
    selection: Option<RemoteLaunchRequest>,
    startup: Option<Arc<SourceStartupBudget>>,
    lease: Option<EngineSourceLease>,
    journal: Option<HeldJournalFile>,
    journal_path: Option<PathBuf>,
    journal_identity: Option<(u64,u64)>,
    issuer: Option<Arc<HostSourceIssuer>>,
    server: Option<SourceAuthorizerServer>,
    settled: Option<SourceAuthorizerSettled>,
    guest: Option<AgentBroker>,
    guest_threads: BrokerThreadOwner,
    guest_settled: Option<BrokerThreadsSettled>,
    peer: Option<Arc<ActivationPeer>>,
    host_token: Option<Secret>, guest_token: Option<Secret>,
    host_revoked: bool, guest_revoked: bool,
    runtime: Option<PreparedContextRuntime>, context: Option<HeldContext>,
    source_paths: Option<OwnedLeaves>, context_paths: Option<OwnedLeaves>,
    permanent: Option<PathBuf>, endpoint: Option<PathBuf>,
}
impl PreparationResources {
    fn new(lease: EngineSourceLease, fence: ManagedLaunchFence) -> Self {
        Self { fence, effects_started:false, issuance_uncertain:false, host_issue:None, guest_issue:None, selection:None, startup:None,
            lease:Some(lease), journal:None, journal_path:None, journal_identity:None,
            issuer:None, server:None, settled:None, guest:None, guest_threads:BrokerThreadOwner::new(),
            guest_settled:None, peer:None, host_token:None, guest_token:None,
            host_revoked:false, guest_revoked:false, runtime:None, context:None,
            source_paths:None, context_paths:None, permanent:None, endpoint:None }
    }
    fn require_complete(&self) -> Result<()> {
        ensure!(self.journal.is_some() && self.journal_identity.is_some() && self.issuer.is_some()
            && self.server.is_some() && self.guest.is_some() && self.peer.is_some()
            && self.host_token.is_some() && self.guest_token.is_some() && self.runtime.is_some()
            && self.context.is_some() && self.source_paths.is_some() && self.context_paths.is_some()
            && self.host_issue.is_some() && self.guest_issue.is_some()
            && !self.issuance_uncertain, "source preparation resource slot incomplete");
        Ok(())
    }
    fn request_retirement(&self) {
        if let Some(startup) = &self.startup { startup.budget.cancel(); }
        if let Some(peer) = &self.peer { peer.0.fail(); }
        if let Some(server) = &self.server { server.close(); }
        if let Some(guest) = &self.guest { guest.close(); }
        self.guest_threads.close();
    }
    fn settle_unused(&mut self, journal: &mut Journal) -> Result<()> {
        self.request_retirement();
        // Even wrong/stale Journal cleanup cannot authorize or discard live
        // handles. Actual owned endpoints may still be closed/joined first.
        if self.guest_settled.is_none() {
            self.guest_settled = Some(self.guest_threads.await_closed(Duration::from_secs(2))?);
        }
        if self.settled.is_none() {
            if let Some(server) = self.server.as_mut() {
                self.settled = Some(server.await_closed(Duration::from_secs(2))?);
            }
        }
        self.journal.as_ref().context("original source Journal anchor absent")?.check_current()?;
        ensure!(self.journal_path.as_deref() == Some(journal.database_path()?),
            "failed preparation cleanup requires original Journal path");
        let supplied = journal.hold_database_file()?;
        let m = fs::symlink_metadata(journal.database_path()?)?;
        ensure!(Some((m.dev(),m.ino())) == self.journal_identity,
            "failed preparation cleanup Journal identity changed");
        supplied.check_current()?;
        self.fence.check_current(journal)?;
        if !self.guest_revoked {
            if let Some(issue) = self.guest_issue.as_mut() { journal.retire_prepared_capability(issue, now_ms()?)?; }
            else if let Some(token) = &self.guest_token { journal.revoke_capability(&self.fence.owner, &token.0, now_ms()?)?; }
            self.guest_revoked = true;
        }
        if !self.host_revoked {
            if let Some(issue) = self.host_issue.as_mut() { journal.retire_prepared_capability(issue, now_ms()?)?; }
            else if let Some(token) = &self.host_token { journal.revoke_capability(&self.fence.owner, &token.0, now_ms()?)?; }
            self.host_revoked = true;
        }
        ensure!(!self.issuance_uncertain || self.host_issue.is_some() || self.guest_issue.is_some(),
            "unknown capability issuance lacks a retained identity; not settled");
        self.issuance_uncertain = false;
        // No pathname, immutable assignment, cache, staging or producer pin is
        // removed. Preserve all durable/uncertain effects for explicit recovery.
        Ok(())
    }
    fn finish(mut self) -> SourceResources {
        // require_complete ran before construction returned. Taking these slots
        // has no fallible IO/callback between transfer and its new owner.
        SourceResources { journal:self.journal.take().unwrap(), journal_identity:self.journal_identity.unwrap(),
            issuer:self.issuer.take().unwrap(), server:self.server.take().unwrap(), guest:self.guest.take().unwrap(),
            guest_threads:self.guest_threads, guest_settled:None,
            peer:self.peer.take().unwrap(), host_token:self.host_token.take().unwrap(), guest_token:self.guest_token.take().unwrap(),
            host_issue:self.host_issue.take().unwrap(), guest_issue:self.guest_issue.take().unwrap(),
            runtime:self.runtime.take().unwrap(), context:self.context.take().unwrap(),
            source_paths:self.source_paths.take().unwrap(), context_paths:self.context_paths.take().unwrap(),
            settled:None, host_revoked:false }
    }
}

/// Existing callers keep the successful API shape, with an owned failure they
/// must explicitly route off-App instead of flattening into a diagnostic Err.
pub(crate) use prepare_owned as prepare;
pub(crate) fn prepare_owned(
    request: ManagedSourcePrepareRequest, journal: &mut Journal, cancelled: &AtomicBool,
) -> std::result::Result<PreparedManagedSource, ManagedSourcePrepareFailure> {
    let ManagedSourcePrepareRequest { lease, fence, selection, inputs } = request;
    let mut pending = PreparationResources::new(lease, fence.clone());
    pending.selection = Some(selection.clone());
    // No live handles are locals inside this unwind boundary after acquisition.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<_> {
    let startup = SourceStartupBudget::begin(inputs.timeout_seconds)?;
    pending.startup = Some(startup.clone());
    let check = || -> Result<()> {
        startup.check_preparation()?;
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "remote source preparation cancelled"
        );
        pending.lease.as_ref().context("source lease absent before issuer")?.check_journal(journal)?;
        fence.check_current(journal)?;
        startup.check_preparation()
    };
    check()?;
    let journal_anchor = journal.hold_database_file()?;
    let metadata = fs::symlink_metadata(journal.database_path()?)?;
    let journal_identity = (metadata.dev(), metadata.ino());
    journal_anchor.check_current()?;
    pending.journal_path = Some(journal.database_path()?.to_path_buf());
    pending.journal = Some(journal_anchor);
    pending.journal_identity = Some(journal_identity);
    let namespace = inputs.managed_store.binding();
    ensure!(
        namespace.project_id == fence.owner.project_id
            && namespace.journal == journal.database_path()?
            && ManagedStore::open(journal, namespace)? == inputs.managed_store,
        "source preparation differs from its privileged prepared project Journal"
    );
    let managed_run = inputs.managed_store.run(&fence.run_id)?;
    let reserved = managed_run
        .reservation(&fence.attempt_id)?
        .context("durable managed reservation absent")?;
    ensure!(
        matches!(reserved, super::ManagedRunSource::Reserved { fence: expected, runnable_root }
        if expected == fence && runnable_root == selection.root.root),
        "remote selection differs from durable reservation"
    );
    let root = namespace
        .directory
        .join("preparations")
        .join(&fence.request_id);
    let endpoint = super::validate_endpoint_parent(&inputs.endpoint_parent, &fence)?;
    pending.permanent = Some(root.clone()); pending.endpoint = Some(endpoint.clone());
    ensure!(
        inputs.context.cache_root == root.join("cache")
            && inputs.context.registry_root == root.join("registry"),
        "source cache and registry require this preparation's private roots"
    );
    if let Some(program) = inputs.command.first() {
        super::validate_runtime_program(program, &inputs.runtime)?;
    }
    let binding = PartialBinding {
        protocol: rudder_engine::vfs::partial_rpc::PROTOCOL.into(),
        format_version: 1,
        launch_request_digest: super::hex_digest(Sha256::digest(canonical_json(&selection)?)),
        root: selection.root.clone(),
        attempt: selection.attempt.clone(),
        observation: selection.observation.clone(),
    };
    binding.validate()?;
    let host_grant = grant(&fence, HOST_SOURCE_REQUIRED_OPERATION,
        host_source_scope(&binding)?, startup.bounds.host_expiry_ms);
    startup.check_preparation()?;
    let issue_time = now_ms()?;
    pending.host_issue = Some(journal.prepare_capability_issuance(&fence.owner, &host_grant, issue_time)?);
    pending.effects_started = true;
    pending.issuance_uncertain = true;
    journal.issue_prepared_capability(pending.host_issue.as_mut().unwrap(), issue_time)?;
    let host_token = Secret(pending.host_issue.as_ref().unwrap().acknowledged_token()?.to_owned());
    pending.host_token = Some(Secret(host_token.0.clone()));
    pending.issuance_uncertain = false;
    let issuer = HostSourceIssuer::new_retaining(&mut pending.lease, binding.clone(), host_token.0.clone())?;
    pending.issuer = Some(issuer.clone());
        startup.check_preparation()?;
        issuer.authorize(&binding)?;
        fence.check_current(journal)?;
        startup.check_preparation()?;
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "source preparation cancelled before effects"
        );
        crate::managed_store::prepare_execution_parent(&inputs.managed_store)?;
        startup.check_preparation()?;
        new_private(&root)?;
        for name in ["context", "cache", "registry", "manager", "store"] {
            startup.check_preparation()?;
            new_private(&root.join(name))?;
        }
        startup.check_preparation()?;
        new_private(&endpoint)?;
        startup.check_preparation()?;
        new_private(&endpoint.join("guest"))?;
        write_new(
            &endpoint.join("owner.json"),
            &canonical_json(
                &serde_json::json!({"format_version":1,"fence":fence,"permanent":root}),
            )?,
        )?;
        startup.check_preparation()?;
        let peer = Arc::new(ActivationPeer(ActivationSlot::with_startup(startup.clone())?));
        pending.peer = Some(peer.clone());
        SourceAuthorizerServer::bind_with_request_peer_retaining(
            &endpoint.join("source"), binding.clone(), issuer.clone(), Limits::default(),
            peer.clone(), &mut pending.server)?;
        let authorizer_directory = pending.server.as_ref().unwrap().directory().to_path_buf();
        startup.check_preparation()?;
        let host_context = remote_workspace::write_context2_checked(
            &root.join("context/source.json"),
            &selection,
            inputs.context,
            &authorizer_directory,
            &|| startup.check_preparation(),
        )?;
        let profile = ExecutionProfileV4 {
            format_version: 1,
            request_id: fence.request_id.clone(),
            startup: Some(StartupConstraint::from_original(startup.budget.constraint(),
                startup.bounds.start_ms)?),
            network: inputs.network,
            workspace: ExecutionWorkspaceSelectionV4::RemoteSourceOnly {
                selection: selection.clone(),
                host_context: host_context.clone(),
            },
        };
        let guest_socket = endpoint.join("guest/broker.sock");
        let spec = ExecutionSpec {
            format_version: 5,
            execution_id: fence.attempt_id.clone(),
            operation_id: selection.attempt.operation_id.clone(),
            view_id: selection.attempt.view_id.clone(),
            owner: fence.owner_epoch(),
            execution_root: root.join("executions"),
            manager_root: root.join("manager"),
            store_root: root.join("store"),
            // Service projections are siblings of the host-only source authority.
            // Declaring their parent would hide the workspace endpoint inside
            // protected state and correctly fail execution root validation.
            authority_roots: vec![
                root.clone(),
                endpoint.join("source"),
                endpoint.join("owner.json"),
                namespace.home.clone(),
            ],
            command: inputs.command,
            environment: BTreeMap::new(),
            home_files: Vec::new(),
            runtime: inputs.runtime,
            broker_directory: Some(endpoint.join("guest")),
            broker_binding: Some(BrokerBinding {
                project_id: fence.owner.project_id.clone(),
                run_id: fence.run_id.clone(),
                run_epoch: fence.run_epoch,
                owner_epoch: fence.owner.epoch,
            }),
            workspace_observation: Some(WorkspaceObservationProfile {
                binding: selection.observation.clone(),
                endpoint_directory: endpoint.join("partial"),
                application: None,
            }),
            profile: Some(profile.clone()),
            timeout_seconds: inputs.timeout_seconds,
        };
        startup.check_preparation()?;
        pending.runtime = Some(remote_workspace::prepare_context2_runtime_checked(
            &host_context,
            &spec,
            &selection,
            &|| startup.check_preparation(),
        )?);
        startup.check_preparation()?;
        pending.context = Some(HeldContext::open_checked(&host_context, &|| startup.check_preparation())?);
        let managed_binding = ManagedExecutionBinding {
            format_version: 4,
            request_id: fence.request_id.clone(),
            run_id: fence.run_id.clone(),
            run_epoch: fence.run_epoch,
            execution_id: spec.execution_id.clone(),
            operation_id: spec.operation_id.clone(),
            view_id: spec.view_id.clone(),
            owner: spec.owner.clone(),
            execution_profile: "offline-command-v1".into(),
            frontend_start: ManagedFrontendStart::Immediate,
            server_command: None,
            guest_socket: "/tmp/cx.sock".into(),
            profile: Some(profile),
        };
        managed_binding.validate(&spec)?;
        let assignment = RemoteSourceAssignment {
            format_version: 2,
            fence: fence.clone(),
            selection,
            host_context,
        };
        assignment.validate(&fence)?;
        // Immutable assignment is durable BEFORE the guest capability is issued.
        startup.check_preparation()?;
        managed_run.persist_source_assignment(&assignment)?;
        startup.check_preparation()?;
        write_new(
            &root.join("assignment2.json"),
            &canonical_json(&assignment)?,
        )?;
        startup.check_preparation()?;
        let guest_grant = grant(&fence, REQUIRED_OPERATION, partial_scope(&binding)?,
            startup.bounds.guest_expiry_ms);
        let issue_time = now_ms()?;
        pending.guest_issue = Some(journal.prepare_capability_issuance(&fence.owner, &guest_grant, issue_time)?);
        pending.issuance_uncertain = true;
        journal.issue_prepared_capability(pending.guest_issue.as_mut().unwrap(), issue_time)?;
        let guest_token = Secret(pending.guest_issue.as_ref().unwrap().acknowledged_token()?.to_owned());
        pending.guest_token = Some(Secret(guest_token.0.clone()));
        pending.issuance_uncertain = false;
        let mut make_guest = || -> Result<AgentBroker> {
            let identity = AgentIdentityFile {
                format_version: 1,
                project_id: fence.owner.project_id.clone(),
                run_id: fence.run_id.clone(),
                run_epoch: fence.run_epoch,
                owner_epoch: fence.owner.epoch,
                token: guest_token.0.clone(),
            };
            let identity_file = agent_broker::project_identity(&guest_socket, &identity)?;
            AgentBroker::spawn_retaining(BrokerConfig {
                journal_path: namespace.journal.clone(), identity_file, socket_path: guest_socket.clone(),
            }, &mut pending.guest_threads)
        };
        pending.guest = Some(make_guest()?);
        // Full filesystem validation requires the actual projected broker
        // identity. Its grant is issued only after the immutable assignment.
        startup.check_preparation()?;
        spec.validate()
            .context("validate prepared source execution")?;
        startup.check_preparation()?;
        pending.runtime.as_mut().unwrap().check_current_checked(&|| startup.check_preparation())?;
        pending.context.as_mut().unwrap().check_current_checked(&|| startup.check_preparation())?;
        issuer.authorize(&binding)?;
        fence.check_current(journal)?;
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "source preparation cancelled after effects"
        );
        pending.source_paths = Some(OwnedLeaves::capture(&authorizer_directory)?);
        pending.context_paths = Some(OwnedLeaves::capture(&root.join("context"))?);
        startup.finish_preparation()?;
        pending.require_complete()?;
        Ok((assignment, spec, managed_binding, managed_run))
    }));
    match result {
        Ok(Ok((assignment, spec, binding, managed_run))) => Ok(PreparedManagedSource {
            assignment, spec, binding, managed_run, adopted:false, resources:Some(pending.finish()),
        }),
        outcome => {
            let cause = match outcome {
                Ok(Err(error)) => error,
                Err(_) => anyhow::anyhow!("source preparation worker panicked"),
                Ok(Ok(_)) => unreachable!(),
            };
            // No-effect failures release only this unused lease/descriptor set.
            // Every effectful failure returns its actual owner, even after panic.
            pending.request_retirement();
            let resources = if pending.effects_started { Some(pending) } else { None };
            Err(ManagedSourcePrepareFailure { _cause:cause, resources })
        }
    }
}

use sha2::{Digest, Sha256};

/// Retain the actual protected context inode through helper shutdown. The
/// reference is an expectation; a replacement pathname cannot refresh custody.
struct HeldContext {
    file: File,
    reference: ProtectedHostContextRef,
    identity: (u64, u64),
}
impl HeldContext {
    fn open_checked(
        reference: &ProtectedHostContextRef,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<Self> {
        check()?;
        reference.validate()?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&reference.path)?;
        let metadata = file.metadata()?;
        let mut result = Self {
            file,
            reference: reference.clone(),
            identity: (metadata.dev(), metadata.ino()),
        };
        result.check_current_checked(check)?;
        Ok(result)
    }
    #[cfg(test)]
    fn check_current(&mut self) -> Result<()> {
        self.check_current_checked(&|| Ok(()))
    }
    fn check_current_checked(&mut self, check: &dyn Fn() -> Result<()>) -> Result<()> {
        check()?;
        let held = self.file.metadata()?;
        let named = fs::symlink_metadata(&self.reference.path)?;
        for metadata in [&held, &named] {
            ensure!(
                metadata.is_file()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.mode() & 0o7777 == 0o600
                    && metadata.nlink() == 1
                    && metadata.len() == self.reference.size_bytes
                    && (metadata.dev(), metadata.ino()) == self.identity,
                "protected source context custody changed"
            );
        }
        self.file.seek(SeekFrom::Start(0))?;
        let mut hash = Sha256::new();
        let mut bytes = [0u8; 4096];
        let mut remaining = self.reference.size_bytes;
        while remaining > 0 {
            check()?;
            let limit = remaining.min(bytes.len() as u64) as usize;
            let count = self.file.read(&mut bytes[..limit])?;
            ensure!(count > 0, "protected source context truncated");
            hash.update(&bytes[..count]);
            bytes.fill(0);
            remaining -= count as u64;
        }
        check()?;
        ensure!(
            super::hex_digest(hash.finalize()) == self.reference.sha256,
            "protected source context bytes changed"
        );
        Ok(())
    }
}

/// Owner-serialized cleanup of exact private flat directories. Never recurse
/// through helper data, shared caches, permanent records or replacement paths.
struct OwnedLeaves {
    root: PathBuf,
    identity: (u64, u64),
    leaves: BTreeMap<PathBuf, (u64, u64)>,
    removed: bool,
}
impl OwnedLeaves {
    fn capture(root: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(root)?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o7777 == 0o700,
            "source cleanup directory changed"
        );
        let mut leaves = BTreeMap::new();
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            ensure!(
                !metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.nlink() == 1,
                "source cleanup leaf changed"
            );
            leaves.insert(path, (metadata.dev(), metadata.ino()));
        }
        Ok(Self {
            root: root.into(),
            identity: (metadata.dev(), metadata.ino()),
            leaves,
            removed: false,
        })
    }
    fn remove(&mut self) -> Result<()> {
        if self.removed {
            return Ok(());
        }
        self.check_current()?;
        for path in self.leaves.keys() {
            fs::remove_file(path)?;
        }
        fs::remove_dir(&self.root)?;
        File::open(self.root.parent().context("source cleanup parent absent")?)?.sync_all()?;
        self.removed = true;
        Ok(())
    }
    fn check_current(&self) -> Result<()> {
        let current = Self::capture(&self.root)?;
        ensure!(
            current.identity == self.identity && current.leaves == self.leaves,
            "source cleanup ownership changed; retain for recovery"
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "managed_source_prepare_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
mod failed_preparation_tests {
    use super::*;
    use crate::source_issuer::SourceIssuerFixture;
    fn pending(fixture: &mut SourceIssuerFixture) -> PreparationResources {
        let fence = ManagedLaunchFence { owner:fixture.fence.clone(), run_id:fixture.binding.attempt.run_id.clone(),
            run_epoch:fixture.binding.attempt.run_epoch, attempt_id:fixture.binding.attempt.execution_id.clone(),
            request_id:uuid::Uuid::new_v4().to_string() };
        let lease = fixture.owner.as_ref().unwrap().source_lease(&fence.owner).unwrap();
        let mut resources = PreparationResources::new(lease, fence);
        let journal = fixture.journal();
        resources.journal = Some(journal.hold_database_file().unwrap());
        resources.journal_path = Some(journal.database_path().unwrap().to_path_buf());
        let m = fs::symlink_metadata(journal.database_path().unwrap()).unwrap();
        resources.journal_identity = Some((m.dev(),m.ino()));
        resources
    }
    #[test]
    fn failed_preparation_carrier_retains_actual_lease_and_capability_through_panic() {
        let mut fixture = SourceIssuerFixture::new(None);
        let mut resources = pending(&mut fixture);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resources.effects_started = true;
            resources.host_token = Some(Secret(fixture.token.clone()));
            panic!("injected preparation failure after effect");
        }));
        assert!(panic.is_err());
        resources.lease.as_ref().unwrap().check_journal(fixture.journal()).unwrap();
        let mut failure = ManagedSourcePrepareFailure { _cause:anyhow::anyhow!("private diagnostic"), resources:Some(resources) };
        assert!(!failure.is_settled());
        failure.settle_unused(fixture.journal()).unwrap();
        assert!(failure.is_settled());
        assert!(!format!("{failure:?}").contains("private diagnostic"));
    }
    #[test]
    fn retained_issuance_cleanup_needs_no_raw_token_and_preserves_other_grants() {
        let mut fixture = SourceIssuerFixture::new(None);
        let mut resources = pending(&mut fixture);
        let original_token = fixture.token.clone();
        let binding = fixture.binding.clone();
        let now = now_ms().unwrap();
        let expected = grant(&resources.fence, HOST_SOURCE_REQUIRED_OPERATION,
            host_source_scope(&binding).unwrap(), now + 60_000);
        resources.host_issue = Some(fixture.journal().prepare_capability_issuance(
            &resources.fence.owner, &expected, now).unwrap());
        resources.effects_started = true;
        resources.issuance_uncertain = true;
        fixture.journal().issue_prepared_capability(resources.host_issue.as_mut().unwrap(), now).unwrap();
        assert!(resources.host_token.is_none());
        resources.settle_unused(fixture.journal()).unwrap();
        let issue = resources.host_issue.as_ref().unwrap();
        assert!(issue.is_retired());
        assert_eq!(fixture.journal().observe_prepared_capability(issue).unwrap(),
            rudder_engine::journal::CapabilityIssuanceObservation::Revoked);
        fixture.journal().authorize_host_source(&original_token, &binding, now).unwrap();
    }
    #[test]
    fn failed_preparation_wrong_journal_and_epoch_keep_owner_for_exact_retry() {
        let mut fixture = SourceIssuerFixture::new(None);
        let mut resources = pending(&mut fixture);
        resources.effects_started = true; resources.host_token = Some(Secret(fixture.token.clone()));
        let mut failure = ManagedSourcePrepareFailure { _cause:anyhow::anyhow!("injected"), resources:Some(resources) };
        let mut foreign = Journal::open(fixture.directory().join("foreign.sqlite")).unwrap();
        assert!(failure.settle_unused(&mut foreign).is_err()); assert!(!failure.is_settled());
        failure.resources.as_mut().unwrap().fence.run_epoch += 1;
        assert!(failure.settle_unused(fixture.journal()).is_err()); assert!(!failure.is_settled());
        failure.resources.as_mut().unwrap().fence.run_epoch -= 1;
        failure.settle_unused(fixture.journal()).unwrap(); assert!(failure.is_settled());
    }
    #[test]
    fn unknown_token_issuance_never_becomes_settled_from_close_or_empty_threads() {
        let mut fixture = SourceIssuerFixture::new(None);
        let mut resources = pending(&mut fixture);
        resources.effects_started = true; resources.issuance_uncertain = true;
        let mut failure = ManagedSourcePrepareFailure { _cause:anyhow::anyhow!("unknown commit"), resources:Some(resources) };
        assert!(failure.settle_unused(fixture.journal()).is_err());
        assert!(!failure.is_settled());
        assert!(failure.diagnostic().contains("issuance uncertain"));
        assert!(failure.resources.as_ref().unwrap().lease.is_some());
    }
}
