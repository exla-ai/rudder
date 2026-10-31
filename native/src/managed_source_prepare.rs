//! Private Spec5 preparation, deliberately absent from App/default dispatch.
//! Call on a bounded worker with an actual lease and privileged current Journal.
//! Records/paths/PIDs cannot construct the lease, peer or joined shutdown proof.
use super::ManagedLaunchFence;
use crate::{
    agent_broker::{self, AgentBroker, BrokerConfig},
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
        host_source_scope, CapabilityGrant, HeldJournalFile, Journal,
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
    peer: Arc<ActivationPeer>,
    host_token: Secret,
    guest_token: Secret,
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
    pub(crate) fn retire_unused(&mut self, journal: &mut Journal) -> Result<()> {
        ensure!(!self.adopted, "adopted source requires joined finalization");
        self.retire(journal)
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
        self.retire(journal)
    }
    fn retire(&mut self, journal: &mut Journal) -> Result<()> {
        self.stop_guest(journal)?;
        let resources = self
            .resources
            .as_mut()
            .context("source preparation retired")?;
        // Never infer settlement from close, cancellation, timeout or a record.
        // A failed wait leaves this owner and its handles available for retry.
        if resources.settled.is_none() {
            resources.settled = Some(resources.server.await_closed(Duration::from_secs(2))?);
        }
        if !resources.host_revoked {
            journal.revoke_capability(
                &self.assignment.fence.owner,
                &resources.host_token.0,
                now_ms()?,
            )?;
            resources.host_revoked = true;
        }
        resources.source_paths.remove()?;
        resources.context_paths.remove()?;
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
pub(crate) fn prepare(
    request: ManagedSourcePrepareRequest,
    journal: &mut Journal,
    cancelled: &AtomicBool,
) -> Result<PreparedManagedSource> {
    let ManagedSourcePrepareRequest {
        lease,
        fence,
        selection,
        inputs,
    } = request;
    let startup = SourceStartupBudget::begin(inputs.timeout_seconds)?;
    let check = || -> Result<()> {
        startup.check_preparation()?;
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "remote source preparation cancelled"
        );
        lease.check_journal(journal)?;
        fence.check_current(journal)?;
        startup.check_preparation()
    };
    check()?;
    let journal_anchor = journal.hold_database_file()?;
    let metadata = fs::symlink_metadata(journal.database_path()?)?;
    let journal_identity = (metadata.dev(), metadata.ino());
    journal_anchor.check_current()?;
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
    // The writable Journal is a privileged parameter, never opened from request
    // DATA. The issuer's fresh read through the ACTUAL lease independently joins
    // the random newly issued grant to the real owned Journal before endpoints.
    startup.check_preparation()?;
    let now = now_ms()?;
    let host_token = Secret(journal.issue_capability(
        &fence.owner,
        &grant(
            &fence,
            HOST_SOURCE_REQUIRED_OPERATION,
            host_source_scope(&binding)?,
            startup.bounds.host_expiry_ms,
        ),
        now,
    )?);
    let issuer = match HostSourceIssuer::new(lease, binding.clone(), host_token.0.clone()) {
        Ok(issuer) => issuer,
        Err(error) => {
            let _ = journal.revoke_capability(&fence.owner, &host_token.0, now_ms()?);
            return Err(error);
        }
    };
    let mut issued_guest = None;
    let result = (|| -> Result<PreparedManagedSource> {
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
        let peer = Arc::new(ActivationPeer(ActivationSlot::with_startup(
            startup.clone(),
        )?));
        let server = SourceAuthorizerServer::bind_with_request_peer(
            &endpoint.join("source"),
            binding.clone(),
            issuer.clone(),
            Limits::default(),
            peer.clone(),
        )?;
        startup.check_preparation()?;
        let host_context = remote_workspace::write_context2_checked(
            &root.join("context/source.json"),
            &selection,
            inputs.context,
            server.directory(),
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
        let mut runtime = remote_workspace::prepare_context2_runtime_checked(
            &host_context,
            &spec,
            &selection,
            &|| startup.check_preparation(),
        )?;
        startup.check_preparation()?;
        let mut held_context =
            HeldContext::open_checked(&host_context, &|| startup.check_preparation())?;
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
        let guest_token = Secret(journal.issue_capability(
            &fence.owner,
            &grant(
                &fence,
                REQUIRED_OPERATION,
                partial_scope(&binding)?,
                startup.bounds.guest_expiry_ms,
            ),
            now_ms()?,
        )?);
        issued_guest = Some(Secret(guest_token.0.clone()));
        let make_guest = || -> Result<AgentBroker> {
            let identity = AgentIdentityFile {
                format_version: 1,
                project_id: fence.owner.project_id.clone(),
                run_id: fence.run_id.clone(),
                run_epoch: fence.run_epoch,
                owner_epoch: fence.owner.epoch,
                token: guest_token.0.clone(),
            };
            let identity_file = agent_broker::project_identity(&guest_socket, &identity)?;
            AgentBroker::spawn(BrokerConfig {
                journal_path: namespace.journal.clone(),
                identity_file,
                socket_path: guest_socket.clone(),
            })
        };
        let guest = match make_guest() {
            Ok(guest) => guest,
            Err(error) => {
                let _ = journal.revoke_capability(&fence.owner, &guest_token.0, now_ms()?);
                return Err(error);
            }
        };
        // Full filesystem validation requires the actual projected broker
        // identity. Its grant is issued only after the immutable assignment.
        startup.check_preparation()?;
        spec.validate()
            .context("validate prepared source execution")?;
        startup.check_preparation()?;
        runtime.check_current_checked(&|| startup.check_preparation())?;
        held_context.check_current_checked(&|| startup.check_preparation())?;
        issuer.authorize(&binding)?;
        fence.check_current(journal)?;
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "source preparation cancelled after effects"
        );
        let source_paths = OwnedLeaves::capture(server.directory())?;
        let context_paths = OwnedLeaves::capture(&root.join("context"))?;
        startup.finish_preparation()?;
        Ok(PreparedManagedSource {
            assignment,
            spec,
            binding: managed_binding,
            managed_run,
            adopted: false,
            resources: Some(SourceResources {
                journal: journal_anchor,
                journal_identity,
                issuer: issuer.clone(),
                server,
                guest,
                peer,
                host_token: Secret(host_token.0.clone()),
                guest_token,
                runtime,
                context: held_context,
                source_paths,
                context_paths,
                settled: None,
                host_revoked: false,
            }),
        })
    })();
    if result.is_err() {
        let _ = journal.revoke_capability(&fence.owner, &host_token.0, now_ms()?);
        if let Some(guest) = issued_guest {
            let _ = journal.revoke_capability(&fence.owner, &guest.0, now_ms()?);
        }
    }
    result
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
mod tests;
