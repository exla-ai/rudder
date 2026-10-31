//! Explicit local launch preparation. Every API doing IO runs off the App loop.
//! Persisted assignments contain no provider material; execution specs remain
//! private in memory and cross the one-use sealed descriptor handoff only.
#[cfg(target_os = "linux")]
#[path = "managed_source_prepare.rs"]
pub(crate) mod source_prepare;
use anyhow::{ensure, Context, Result};
use rudder_engine::{
    execution::{ExecutionSpec, ProtectedHostContextRef, HomeFile, ReadOnlyRuntime,
        BrokerBinding, WorkspaceObservationProfile, HelperApplicationSpec},
    journal::{Journal, OwnerFence},
    protocol::{canonical_json, ObjectKind, ObjectRef, OwnerEpoch},
    vfs::{rpc::{ObservationBinding, AttemptContext}, VfsManager, VfsCheckpoint},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs::{self, File, OpenOptions}, io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf},
    sync::{atomic::{AtomicBool, Ordering}, Arc, OnceLock, mpsc::{self, SyncSender}}};
use rudder_vcs_bridge::{seed::{VerifiedMetadataTemplate, fork_private_metadata},
    vfs_provider::SourceProjectionPolicy, command_transport::CommandServiceConfig,
    rpc_view::RpcViewConfig};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedLaunchFence {
    pub owner: OwnerFence,
    pub run_id: String,
    pub run_epoch: u64,
    pub attempt_id: String,
    pub request_id: String,
}
impl ManagedLaunchFence {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(!self.run_id.is_empty() && self.run_id.len() <= 256
            && !self.run_id.chars().any(char::is_control) && !self.run_id.contains(['/', '\\'])
            && !matches!(self.run_id.as_str(), "." | "..") && self.run_epoch > 0,
            "invalid reserved managed run");
        self.owner_epoch().validate()?;
        for id in [&self.attempt_id, &self.request_id] {
            ensure!(uuid::Uuid::parse_str(id).is_ok_and(|u| u.to_string() == *id),
                "managed launch identity must be an App-reserved canonical UUID");
        }
        Ok(())
    }
    pub(crate) fn owner_epoch(&self) -> OwnerEpoch {
        OwnerEpoch { owner: self.owner.owner_id.clone(), epoch: self.owner.epoch }
    }
    pub(crate) fn check_current(&self, journal: &Journal) -> Result<()> {
        self.validate()?;
        ensure!(journal.current_run_attempt(&self.owner, &self.run_id, self.run_epoch)? == self.attempt_id,
            "managed launch reservation is stale");
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceAssignment {
    pub format_version: u32,
    pub fence: ManagedLaunchFence,
    pub runnable_root: ObjectRef,
    pub source: ObjectRef,
    pub seed: ObjectRef,
    pub observation: ObservationBinding,
    pub host_context: ProtectedHostContextRef,
}
impl WorkspaceAssignment {
    pub(crate) fn validate(&self, fence: &ManagedLaunchFence) -> Result<()> {
        fence.validate()?;
        self.observation.validate()?;
        self.host_context.validate()?;
        for reference in [&self.runnable_root, &self.source, &self.seed] { reference.validate()?; }
        ensure!(self.format_version == 1 && self.fence == *fence
            && self.runnable_root.kind == ObjectKind::RunnableRoot
            && self.source.kind == ObjectKind::SourceRoot && self.seed.kind == ObjectKind::Directory
            && self.observation.attempt.execution_id == fence.attempt_id
            && self.observation.owner == fence.owner_epoch(),
            "workspace assignment differs from the exact reserved attempt");
        Ok(())
    }
}

/// Durable source identity only. Restored records are recovery evidence, never
/// a live owner grant or permission to reopen a checkout-backed conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ManagedRunSource {
    Reserved { fence: ManagedLaunchFence, runnable_root: ObjectRef },
    Assigned { assignment: Box<WorkspaceAssignment> },
    /// A malformed persisted marker remains visibly managed and quarantined.
    /// It cannot be supplied as an alternate valid public launch format.
    #[serde(skip_deserializing)]
    Quarantined,
}
impl ManagedRunSource {
    pub(crate) fn validate(&self, run_id: &str) -> Result<()> {
        let fence = match self {
            Self::Reserved { fence, runnable_root } => {
                runnable_root.validate()?;
                ensure!(runnable_root.kind == ObjectKind::RunnableRoot, "managed reservation has no RunnableRoot");
                fence
            }
            Self::Assigned { assignment } => { assignment.validate(&assignment.fence)?; &assignment.fence }
            Self::Quarantined => anyhow::bail!("managed source marker requires explicit recovery"),
        };
        fence.validate()?;
        ensure!(fence.run_id == run_id, "managed source marker belongs to another run"); Ok(())
    }
    pub(crate) fn fence(&self) -> Option<&ManagedLaunchFence> {
        match self { Self::Reserved { fence, .. } => Some(fence), Self::Assigned { assignment } => Some(&assignment.fence), Self::Quarantined => None }
    }
    pub(crate) fn matches_reservation(&self, reservation: &Self) -> bool {
        let Self::Reserved { fence, runnable_root } = reservation else { return false; };
        match self { Self::Reserved { fence:actual, runnable_root:root } => actual == fence && root == runnable_root,
            Self::Assigned { assignment } => assignment.fence == *fence && assignment.runnable_root == *runnable_root,
            Self::Quarantined => false }
    }
}

/// Off-App, before any preparation effects. This immutable recovery marker is
/// fsynced independently of the UI's replaceable run.json projection, which
/// cannot by itself certify crash durability. It never grants live authority.
#[cfg(test)]
pub(crate) fn persist_reservation(path: &Path, source: &ManagedRunSource) -> Result<()> {
    let ManagedRunSource::Reserved { fence, .. } = source else { anyhow::bail!("initial managed sidecar must be a reservation"); };
    source.validate(&fence.run_id)?;
    let directory = path.parent().context("managed reservation directory absent")?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)?;
    let metadata = fs::symlink_metadata(directory)?;
    ensure!(metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() }, "managed reservation directory is not owned");
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    let bytes = canonical_json(source)?;
    match OpenOptions::new().create_new(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path) {
        Ok(mut file) => { file.write_all(&bytes)?; file.sync_all()?; }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(read_reservation(path)? == Some(source.clone()), "managed reservation already belongs to another request");
            File::open(path)?.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    // run, runs, .rudder and repository directory entries may all be new. No
    // filesystem barrier runs on the App actor or in a prepared result's Drop.
    for directory in directory.ancestors().take(4) { File::open(directory)?.sync_all()?; }
    Ok(())
}
pub(crate) fn read_reservation(path: &Path) -> Result<Option<ManagedRunSource>> {
    let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK).open(path) {
        Ok(file) => file, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(error) => return Err(error.into()) };
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() } && metadata.nlink() == 1
        && metadata.mode() & 0o077 == 0 && metadata.len() <= 64 * 1024, "unsafe managed reservation sidecar");
    let mut bytes = Vec::new(); file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    let source: ManagedRunSource = serde_json::from_slice(&bytes)?;
    let ManagedRunSource::Reserved { fence, .. } = &source else { anyhow::bail!("invalid managed reservation sidecar phase"); };
    source.validate(&fence.run_id)?;
    ensure!(canonical_json(&source)? == bytes, "noncanonical managed reservation sidecar");
    Ok(Some(source))
}

pub(crate) fn assignment_path(home: &Path, fence: &ManagedLaunchFence) -> Result<PathBuf> {
    fence.validate()?;
    Ok(crate::managed_store::run_directory(home, &fence.owner.project_id, &fence.run_id)
        .join("attempts").join(&fence.attempt_id).join("assignment.json"))
}
fn hex_digest(bytes: impl AsRef<[u8]>) -> String { bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect() }

/// Off-App. Persist BEFORE granting workspace access. An existing assignment
/// can only reconcile the identical request; it cannot widen a running token.
pub(crate) fn persist_assignment(home: &Path, assignment: &WorkspaceAssignment) -> Result<()> {
    assignment.validate(&assignment.fence)?;
    let store = crate::managed_store::open_for_project(home, &assignment.fence.owner.project_id)?
        .context("managed assignment requires explicit private namespace preparation")?;
    store.run(&assignment.fence.run_id)?.persist_assignment(assignment)
}
/// Off-App. Missing on an ordinary unprepared project means legacy. A managed
/// floor/namespace mismatch is an error, never a fallback to the old sidecar.
pub(crate) fn read_assignment(home: &Path, fence: &ManagedLaunchFence) -> Result<Option<WorkspaceAssignment>> {
    read_assignment_record(home, fence, true)
}
pub(crate) fn assignment_for_attempt(home: &Path, owner: &OwnerFence, run_id: &str, run_epoch: u64, attempt: &str) -> Result<Option<WorkspaceAssignment>> {
    let key = ManagedLaunchFence { owner: owner.clone(), run_id: run_id.into(), run_epoch,
        attempt_id: attempt.into(), request_id: attempt.into() };
    read_assignment_record(home, &key, false)
}
/// Bounded immutable-file read for an already-owned App inventory. The caller
/// supplies its current Journal-derived owner/run/attempt; the location was
/// established off-App. Never open/init another journal or infer an identity.
pub(crate) fn assignment_for_location(location: &crate::managed_store::ManagedRunLocation,
    owner: &OwnerFence, run_id: &str, run_epoch: u64, attempt: &str) -> Result<Option<WorkspaceAssignment>> {
    ensure!(location.run_id()==run_id && location.store().binding().project_id==owner.project_id,
        "managed inventory location belongs to another project/run");
    let assignment=location.assignment(attempt)?;
    if let Some(assignment)=&assignment {
        ensure!(assignment.fence.owner==*owner && assignment.fence.run_id==run_id
            && assignment.fence.run_epoch==run_epoch && assignment.fence.attempt_id==attempt,
            "managed inventory assignment differs from current owned attempt");
    }
    Ok(assignment)
}
fn read_assignment_record(home: &Path, fence: &ManagedLaunchFence, exact_request: bool) -> Result<Option<WorkspaceAssignment>> {
    let Some(store) = crate::managed_store::open_for_project(home, &fence.owner.project_id)? else { return Ok(None); };
    let path = assignment_path(home, fence)?;
    match fs::symlink_metadata(&path) { Ok(_) => {}, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(error) => return Err(error.into()) }
    let assignment = store.run(&fence.run_id)?.assignment(&fence.attempt_id)?.context("managed assignment disappeared")?;
    ensure!(assignment.fence.owner == fence.owner && assignment.fence.run_id == fence.run_id
        && assignment.fence.run_epoch == fence.run_epoch && assignment.fence.attempt_id == fence.attempt_id
        && (!exact_request || assignment.fence == *fence), "workspace assignment is stale");
    Ok(Some(assignment))
}

/// No Debug/Serialize implementation: values belong only in a sealed spec.
pub(crate) struct SelectedCredentials {
    profile_id: String,
    backend: super::Backend,
    environment: std::collections::BTreeMap<String, String>,
}

/// One selected host-owned key. Never Debug/Serde, guest env, home JSON or CAS.
pub(crate) struct ManagedCodexApiKey(String);
impl ManagedCodexApiKey {
    pub(crate) fn expose_for_login(&self) -> &str { &self.0 }
}
impl Drop for ManagedCodexApiKey {
    fn drop(&mut self) { unsafe { self.0.as_bytes_mut().fill(0); } }
}
impl SelectedCredentials {
    /// The selected profile loader supplies exactly one key, never its HOME or
    /// inherited environment. API keys have no refresh/writeback semantics.
    pub(crate) fn api_key(profile_id: String, backend: super::Backend, secret: String) -> Result<Self> {
        ensure!(!profile_id.is_empty() && profile_id.len() <= 256 && !profile_id.chars().any(char::is_control), "invalid selected credential profile");
        ensure!(!secret.is_empty() && secret.len() <= 16 * 1024 && !secret.contains(['\0', '\r', '\n']), "invalid selected provider credential");
        let key = match backend {
            super::Backend::Codex => "OPENAI_API_KEY",
            super::Backend::Claude => "ANTHROPIC_API_KEY",
            _ => anyhow::bail!("managed provider profile is unsupported"),
        };
        Ok(Self { profile_id, backend, environment: [(key.into(), secret)].into() })
    }
    /// Ordinary CLI profiles do not participate in a shared refresh lease.
    /// Refuse before reading/copying a refresh token or invalidating a live login.
    pub(crate) fn oauth_profile(_profile_id: &str) -> Result<Self> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported,
            "managed OAuth requires a qualified exclusive refresh and protected writeback adapter").into())
    }
    pub(crate) fn take_host_codex_key(&mut self) -> Result<Option<ManagedCodexApiKey>> {
        if self.backend != super::Backend::Codex { return Ok(None); }
        ensure!(self.environment.len() == 1, "managed Codex requires exactly its selected API key");
        Ok(Some(ManagedCodexApiKey(self.environment.remove("OPENAI_API_KEY")
            .context("selected Codex profile has no API key")?)))
    }
}
impl Drop for SelectedCredentials {
    fn drop(&mut self) {
        for value in self.environment.values_mut() { unsafe { value.as_bytes_mut().fill(0); } }
    }
}

/// Explicit backend process layout. Both commands contain guest paths only.
/// The helper launches the server directly and reports its held kernel identity.
pub(crate) enum ManagedBackendProfile {
    CodexAppServer { program: String, arguments: Vec<String> },
    ClaudeSdk { node: String, rudder_cli: String, claude: String,
        arguments: Vec<String>, initial_prompt: Option<String> },
}
impl ManagedBackendProfile {
    pub(crate) fn backend(&self) -> super::Backend {
        match self { Self::CodexAppServer { .. } => super::Backend::Codex,
            Self::ClaudeSdk { .. } => super::Backend::Claude }
    }
    pub(crate) fn execution_profile(&self) -> &'static str {
        match self { Self::CodexAppServer { .. } => "codex-app-server",
            Self::ClaudeSdk { .. } => "claude-sdk" }
    }
}

pub(crate) struct ManagedLocalLaunchRequest {
    pub fence: ManagedLaunchFence,
    pub journal: PathBuf,
    pub home: PathBuf,
    pub managed_store: crate::managed_store::ManagedStore,
    pub store: rudder_engine::store::LocalStore,
    pub runnable_root: ObjectRef,
    /// Trusted accepted-history binding. Never inferred from a checkout/cwd.
    pub seed_source: ObjectRef,
    pub seed: VerifiedMetadataTemplate,
    pub operation_id: String,
    pub view_id: String,
    /// New owned directory, ending in the reserved request UUID.
    pub private_root: PathBuf,
    pub endpoint_parent: PathBuf,
    pub backend: ManagedBackendProfile,
    pub credentials: SelectedCredentials,
    /// Trusted host selection only. These paths are denied to every runtime
    /// projection; no traversal, credential lookup or guest copy is implied.
    pub protected_credential_roots: Vec<PathBuf>,
    pub runtime: Vec<ReadOnlyRuntime>,
    pub native_guest: String,
    pub jj_guest: String,
    pub user_name: String,
    pub user_email: String,
    pub timeout_seconds: u64,
}

/// An explicit retained-root launch selection, without any run UUIDs. The App
/// reserves all identities before moving this value to the bounded IO worker.
/// Construct/verify retained seed/runtime/credential values off-App; this type
/// performs no profile lookup, environment scan or checkout inference.
pub(crate) struct ManagedLocalLaunchInputs {
    pub journal: PathBuf,
    pub home: PathBuf,
    pub managed_store: crate::managed_store::ManagedStore,
    pub store: rudder_engine::store::LocalStore,
    pub runnable_root: ObjectRef,
    pub seed_source: ObjectRef,
    pub seed: VerifiedMetadataTemplate,
    pub preparation_parent: PathBuf,
    /// Explicit short, canonical UID-owned0700 parent for transient sockets.
    pub endpoint_parent: PathBuf,
    pub backend: ManagedBackendProfile,
    pub credentials: SelectedCredentials,
    pub protected_credential_roots: Vec<PathBuf>,
    pub runtime: Vec<ReadOnlyRuntime>,
    pub native_guest: String,
    pub jj_guest: String,
    pub user_name: String,
    pub user_email: String,
    pub timeout_seconds: u64,
}
impl ManagedLocalLaunchInputs {
    pub(crate) fn bind(self, fence: ManagedLaunchFence, operation_id: String, view_id: String) -> ManagedLocalLaunchRequest {
        ManagedLocalLaunchRequest { private_root: self.preparation_parent.join(&fence.request_id), fence,
            journal: self.journal, home: self.home, store: self.store, runnable_root: self.runnable_root,
            managed_store:self.managed_store,
            endpoint_parent:self.endpoint_parent,
            seed_source: self.seed_source, seed: self.seed, operation_id, view_id, backend: self.backend,
            credentials: self.credentials, protected_credential_roots: self.protected_credential_roots,
            runtime: self.runtime, native_guest: self.native_guest,
            jj_guest: self.jj_guest, user_name: self.user_name, user_email: self.user_email, timeout_seconds: self.timeout_seconds }
    }
}

/// Credential-free, explicit path mapping. A pathname is never readiness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedTransportMapping {
    pub host_socket: PathBuf,
    pub guest_socket: PathBuf,
    pub guest_cwd: PathBuf,
}

/// Guest-owned candidate reports. These paths convey no backend/process,
/// acknowledgment, writer-barrier or checkpoint authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedReportMapping {
    pub host_done_file: PathBuf,
    pub guest_done_file: PathBuf,
    pub host_signal_file: PathBuf,
    pub guest_signal_file: PathBuf,
}

struct ManagedEndpoints { root:PathBuf, marker:Vec<u8>, backend_target:PathBuf }
fn validate_endpoint_parent(parent: &Path, fence: &ManagedLaunchFence) -> Result<PathBuf> {
    fence.validate()?;
    ensure!(parent.is_absolute() && parent.canonicalize()? == parent, "managed endpoint parent must be explicit/canonical");
    let meta=fs::symlink_metadata(parent)?;
    ensure!(meta.is_dir() && meta.uid()==unsafe { libc::geteuid() } && meta.mode() & 0o777 == 0o700,
        "managed endpoint parent must be UID-owned0700");
    let root=parent.join(&fence.request_id);
    for path in [root.join("backend.sock"),root.join("workspace/workspace.sock"),root.join("vcs/jj.sock")] {
        ensure!(path.as_os_str().as_encoded_bytes().len()<100,"managed transient socket exceeds portable path bound");
    }
    Ok(root)
}
fn prepare_endpoints(parent: &Path, fence: &ManagedLaunchFence, permanent: &Path, backend_target:PathBuf) -> Result<ManagedEndpoints> {
    let root=validate_endpoint_parent(parent,fence)?;
    fs::DirBuilder::new().mode(0o700).create(&root)?;
    let marker=canonical_json(&serde_json::json!({"format_version":1,"fence":fence,"permanent":permanent,"backend_target":backend_target}))?;
    let mut file=OpenOptions::new().create_new(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(root.join("owner.json"))?;
    file.write_all(&marker)?; file.sync_all()?;
    std::os::unix::fs::symlink(&backend_target,root.join("backend.sock"))?;
    File::open(&root)?.sync_all()?; File::open(parent)?.sync_all()?;
    Ok(ManagedEndpoints { root,marker,backend_target })
}
fn retire_endpoints(endpoints:ManagedEndpoints) -> Result<()> {
    let parent=endpoints.root.parent().context("managed endpoint parent absent")?;
    ensure!(parent.canonicalize()? == parent,"managed endpoint parent changed");
    for directory in [parent,endpoints.root.as_path()] {
        let meta=fs::symlink_metadata(directory)?; ensure!(meta.is_dir() && meta.uid()==unsafe { libc::geteuid() }
            && meta.mode() & 0o777 == 0o700,"managed endpoint ownership changed");
    }
    let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(endpoints.root.join("owner.json"))?;
    let meta=file.metadata()?; ensure!(meta.is_file() && meta.len()<=64*1024 && meta.uid()==unsafe { libc::geteuid() } && meta.mode() & 0o077==0,"managed endpoint marker changed");
    let mut marker=Vec::new(); file.take(64*1024+1).read_to_end(&mut marker)?;
    ensure!(marker==endpoints.marker && fs::read_link(endpoints.root.join("backend.sock"))?==endpoints.backend_target,
        "managed endpoint binding changed; retain for recovery");
    fs::remove_dir_all(&endpoints.root)?; File::open(parent)?.sync_all()?; Ok(())
}
impl ManagedReportMapping {
    fn for_execution(home: &Path) -> Self {
        Self { host_done_file: home.join(".rudder-reports/done.json"),
            guest_done_file: "/home/agent/.rudder-reports/done.json".into(),
            host_signal_file: home.join(".rudder-reports/signal.json"),
            guest_signal_file: "/home/agent/.rudder-reports/signal.json".into() }
    }
}

pub(crate) struct PreparedManagedLaunch {
    pub assignment: WorkspaceAssignment,
    pub transport: ManagedTransportMapping,
    pub reports: ManagedReportMapping,
    pub execution_profile: String,
    pub identity_home: PathBuf,
    pub identity_journal: PathBuf,
    pub managed_run: crate::managed_store::ManagedRunLocation,
    pub server_command: Option<Vec<String>>,
    codex_api_key: Option<ManagedCodexApiKey>,
    spec: Option<ExecutionSpec>,
    retirement: Option<Retirement>,
}
impl PreparedManagedLaunch {
    pub(crate) fn take_codex_api_key(&mut self) -> Option<ManagedCodexApiKey> { self.codex_api_key.take() }
    /// App calls only after the exact checkpoint receipt AND actual PTY exit.
    /// This queues transient endpoint cleanup on Drop; permanent data/pins stay.
    pub(crate) fn confirm_transient_settlement(&mut self) { if let Some(retirement)=&mut self.retirement { retirement.endpoints_safe=true; } }
    #[cfg(target_os = "linux")]
    pub(crate) fn prepare_handoff(&self) -> Result<rudder_engine::execution::handoff::ManagedExecutionHandoff> {
        let spec = self.execution_spec()?;
        rudder_engine::execution::handoff::ManagedExecutionHandoff::new(spec,
            rudder_engine::execution::ManagedExecutionBinding { format_version: 2,
                profile: None,
                request_id: self.assignment.fence.request_id.clone(), run_id: self.assignment.fence.run_id.clone(),
                run_epoch: self.assignment.fence.run_epoch, execution_id: spec.execution_id.clone(),
                operation_id: spec.operation_id.clone(), view_id: spec.view_id.clone(), owner: spec.owner.clone(),
                execution_profile: self.execution_profile.clone(), server_command: self.server_command.clone(),
                frontend_start: if self.execution_profile == "codex-app-server" { rudder_engine::execution::ManagedFrontendStart::AfterHostApiKeyV1 }
                    else { rudder_engine::execution::ManagedFrontendStart::Immediate },
                guest_socket: self.transport.guest_socket.clone() })
    }
    pub(crate) fn link_launch(&self) -> crate::agent_link::AgentLinkLaunch {
        crate::agent_link::AgentLinkLaunch { format_version: 1, run_id: self.assignment.fence.run_id.clone(),
            attempt_id: self.assignment.fence.attempt_id.clone(), backend: if self.execution_profile == "codex-app-server" { "codex".into() } else { self.execution_profile.clone() },
            socket_path: self.transport.host_socket.clone(), log_path: self.transport.host_socket.with_file_name("backend.log") }
    }
    /// Explicit unused-result retirement on a background worker. Adopted or
    /// uncertain running attempts retain their recovery state conservatively.
    pub(crate) fn retire(mut self) -> Result<()> {
        if let Some(retirement) = self.retirement.take() {
            ensure!(!retirement.adopted, "adopted managed execution requires receipt reconciliation");
            retire_unused(retirement)?;
        }
        Ok(())
    }
    /// Off-App. Called only after adopting this result and persisting its exact
    /// assignment. Capability projection must precede sealed-spec validation.
    pub(crate) fn execution_spec(&self) -> Result<&ExecutionSpec> {
        self.spec.as_ref().context("managed launch already consumed")
    }
    /// App adoption moves ownership without IO. Caller keeps this object until
    /// the matched helper record confirms termination; Drop does not retire a
    /// live adopted view or revoke authority needed for final checkpoint.
    pub(crate) fn adopt(&mut self) {
        if let Some(retirement) = &mut self.retirement { retirement.adopted = true; }
    }
}
struct Retirement {
    root: PathBuf,
    marker: Vec<u8>,
    pins: Vec<String>,
    adopted: bool,
    endpoints:Option<ManagedEndpoints>,
    endpoints_safe:bool,
}
fn retire_unused(retirement: Retirement) -> Result<()> {
    if retirement.adopted && !retirement.endpoints_safe { return Ok(()); }
    let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(retirement.root.join("preparation.json"))?;
    ensure!(file.metadata()?.is_file() && file.metadata()?.len()<=64*1024,"managed preparation marker exceeds bound");
    let mut marker=Vec::new(); file.take(64*1024+1).read_to_end(&mut marker)?;
    ensure!(marker == retirement.marker,
        "managed preparation retirement identity changed");
    // Permanent history, workspace/checkpoints, reservation and pins survive
    // every automatic cleanup. Only exact owned transient endpoints retire.
    if let Some(endpoints)=retirement.endpoints { retire_endpoints(endpoints)?; }
    Ok(())
}
fn retirement_queue() -> Option<&'static SyncSender<Retirement>> {
    static QUEUE: OnceLock<Option<SyncSender<Retirement>>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (send, receive) = mpsc::sync_channel::<Retirement>(32);
        std::thread::Builder::new().name("rudder-managed-retirement".into()).spawn(move || {
            while let Ok(retirement) = receive.recv() { let _ = retire_unused(retirement); }
        }).ok().map(|_| send)
    }).as_ref()
}
impl Drop for PreparedManagedLaunch {
    fn drop(&mut self) {
        // No filesystem, database or child waits on the App thread. On bounded
        // queue exhaustion the durable preparation marker and pins remain for
        // explicit reconciliation; no success/retirement is inferred.
        if let Some(retirement) = self.retirement.take().filter(|r| !r.adopted || r.endpoints_safe) {
            if let Some(queue) = retirement_queue() { let _ = queue.try_send(retirement); }
        }
        if let Some(spec) = &mut self.spec {
            for value in spec.environment.values_mut() { unsafe { value.as_bytes_mut().fill(0); } }
            for file in &mut spec.home_files { file.contents.fill(0); }
        }
    }
}

/// Blocking IO only. No App access, PTY spawn, capability grant, run observation
/// or provider call. Every identity was reserved by the App before invocation.
pub(crate) fn prepare_local(mut request: ManagedLocalLaunchRequest, cancelled: Arc<AtomicBool>) -> Result<PreparedManagedLaunch> {
    ensure!(cfg!(target_os = "linux"), "managed local execution requires a qualified Linux worker");
    let journal = Journal::open(&request.journal)?;
    let managed_run = request.managed_store.run(&request.fence.run_id)?;
    ensure!(request.managed_store.binding().home == request.home
        && request.managed_store.binding().journal == request.journal
        && request.managed_store.binding().project_id == request.fence.owner.project_id
        && crate::managed_store::ManagedStore::open(&journal, request.managed_store.binding())? == request.managed_store,
        "managed preparation differs from explicit prepared project identity");
    let reserved = managed_run.reservation(&request.fence.attempt_id)?.context("durable private managed reservation missing")?;
    ensure!(matches!(&reserved, ManagedRunSource::Reserved { fence, runnable_root }
        if fence == &request.fence && runnable_root == &request.runnable_root), "managed preparation differs from durable reservation");
    ensure!(!crate::gitio::native_run_dir(&request.managed_store.binding().repository, &request.fence.run_id).try_exists()?,
        "legacy run projection exists; explicit managed recovery required");
    validate_endpoint_parent(&request.endpoint_parent,&request.fence)?;
    let check = || -> Result<()> {
        ensure!(!cancelled.load(Ordering::Acquire), "managed launch preparation cancelled");
        request.fence.check_current(&journal)
    };
    check()?;
    for id in [&request.operation_id, &request.view_id] {
        ensure!(uuid::Uuid::parse_str(id).is_ok_and(|u| u.to_string() == *id), "managed workspace UUID was not explicitly reserved");
    }
    ensure!(request.private_root.is_absolute() && request.private_root.components()
        .all(|component| matches!(component, std::path::Component::RootDir | std::path::Component::Normal(_)))
        && request.private_root.file_name()
        .is_some_and(|n| n == request.fence.request_id.as_str()), "managed private root must bind its reserved request");
    ensure!(request.private_root.parent() == Some(request.managed_store.binding().directory.join("preparations").as_path()),
        "managed preparation must remain inside its permanent private namespace");
    ensure!((1..=24 * 60 * 60).contains(&request.timeout_seconds), "invalid managed execution timeout");
    ensure!(request.backend.backend() == request.credentials.backend,
        "selected credential profile belongs to another backend");
    let protected_roots = selected_protected_roots(&request.home, &request.protected_credential_roots)?;
    let codex_api_key = request.credentials.take_host_codex_key()?;
    let mut programs = vec![request.native_guest.as_str(), request.jj_guest.as_str()];
    match &request.backend {
        ManagedBackendProfile::CodexAppServer { program, .. } => programs.push(program),
        ManagedBackendProfile::ClaudeSdk { node, rudder_cli, claude, .. } => programs.extend([node.as_str(), rudder_cli.as_str(), claude.as_str()]),
    }
    for program in programs { validate_runtime_program(program, &request.runtime)?; }
    ensure!(request.runnable_root.kind == ObjectKind::RunnableRoot, "managed local launch requires a RunnableRoot");
    let seed_ref = request.seed.retained_seed().context("managed local launch requires a retained private seed")?.clone();
    let retained = request.store.retention()?;
    for root in [&request.runnable_root, &seed_ref] {
        ensure!(retained.pins.values().any(|r| r == root), "managed launch input has no retained LOCAL authority");
    }
    let runnable = rudder_engine::snapshot::decode_runnable(&request.store.read_object(&request.runnable_root)?)?;
    ensure!(request.seed_source == runnable.source, "private VCS seed belongs to another accepted source");
    let source = rudder_engine::snapshot::decode_source(&request.store.read_object(&runnable.source)?)?;
    let profile: rudder_engine::workspace::WorkspaceProfile = serde_json::from_slice(&request.store.read_object(&runnable.profile)?)?;
    let policy = SourceProjectionPolicy::from_import_policy(&profile.source_exclusions())?;
    ensure!(request.store.read_object(&source.projection_policy)? == canonical_json(&policy)?,
        "managed source policy differs from its selected workspace profile");
    check()?;
    crate::managed_store::prepare_execution_parent(&request.managed_store)?;
    fs::DirBuilder::new().mode(0o700).create(&request.private_root)?;
    let marker = canonical_json(&serde_json::json!({"format_version":1,"fence":request.fence,
        "runnable_root":request.runnable_root,"seed":seed_ref,"operation_id":request.operation_id,"view_id":request.view_id,
        "endpoint_parent":request.endpoint_parent,"endpoint_root":request.endpoint_parent.join(&request.fence.request_id),"phase":"preparing"}))?;
    let mut file = OpenOptions::new().create_new(true).write(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(request.private_root.join("preparation.json"))?;
    file.write_all(&marker)?; file.sync_all()?; File::open(&request.private_root)?.sync_all()?;
    let mut retirement = Retirement { root: request.private_root.clone(), marker,
        pins: Vec::new(), adopted: false,endpoints:None,endpoints_safe:false };
    let prepared = (|| -> Result<PreparedManagedLaunch> {
        for (kind, root) in [("runnable", &request.runnable_root), ("seed", &seed_ref)] {
            let pin = format!("managed:{}:{kind}", request.fence.request_id);
            request.store.pin_existing(&pin, root)?; retirement.pins.push(pin);
        }
        let manager = VfsManager::open(request.private_root.join("vfs"), request.store.clone())?;
        let snapshot = VfsCheckpoint { format_version: 1, directory: runnable.view_root.clone(), hardlinks: runnable.hardlinks.clone() };
        manager.prepare_paged_snapshot(&snapshot)?;
        let view = manager.fork_paged_snapshot(&request.view_id, &snapshot, request.fence.owner_epoch())?;
        retirement.pins.extend([format!("vfs:view:{}:base", request.view_id), format!("vfs:view:{}:topology", request.view_id)]);
        check()?;
        let observation = ObservationBinding { format_version: 1,
            attempt: AttemptContext { execution_id: request.fence.attempt_id.clone(), operation_id: request.operation_id.clone() },
            view_id: request.view_id.clone(), owner: request.fence.owner_epoch(), namespace: view.dirty_cursor()?.namespace,
            profile: runnable.profile.clone(), exclusion_policy_digest: rudder_engine::vfs::exclusion_policy_digest(&profile.source_exclusions())? };
        let binding = rudder_vcs_bridge::AttemptBinding { format_version: 1, project_id: request.fence.owner.project_id.clone(),
            owner: request.fence.owner_epoch(), run_id: request.fence.run_id.clone(), run_epoch: request.fence.run_epoch,
            attempt_id: request.fence.attempt_id.clone(), view_id: request.view_id.clone(), seed: seed_ref.clone(),
            baseline: request.seed.manifest().baseline.clone(), baseline_operation: request.seed.manifest().operation.clone(),
            projection_policy: policy.reference()?, profile: runnable.profile.clone() };
        let private = request.private_root.join("private");
        // A local seed with LocalWC metadata is rebound by the post-RPC host
        // factory. Already-bound Rudder metadata needs a fresh observation and
        // is explicitly refused here rather than inventing one before RPC.
        fork_private_metadata(&request.seed, &private, binding.clone(), None)?;
        check()?;
        for name in ["context", "state", "projections"] {
            fs::DirBuilder::new().mode(0o700).create(request.private_root.join(name))?;
        }
        let execution_root = request.private_root.join("e");
        let backend_target = execution_root.join("runs").join(&request.fence.attempt_id).join("tmp/cx.sock");
        retirement.endpoints=Some(prepare_endpoints(&request.endpoint_parent,&request.fence,&request.private_root,backend_target)?);
        let transient=&retirement.endpoints.as_ref().unwrap().root;
        let endpoint = transient.join("workspace");
        let service = CommandServiceConfig { format_version: 1,
            view: RpcViewConfig { format_version: 1, attempt: binding, observation: observation.clone(),
                socket: endpoint.join("workspace.sock"), identity_file: endpoint.join("identity.json"),
                private_repository: private.clone(), private_git: private.join(".git"),
                projection_state: request.private_root.join("projections"), cas_root: request.store.root().into(),
                policy, budget: Default::default() }, binary: PathBuf::new(), state_directory: request.private_root.join("state"),
            git_config_sha256: hex_digest(Sha256::digest(fs::read(private.join(".git/config"))?)),
            user_name: request.user_name.clone(), user_email: request.user_email.clone(), command_timeout_ms: 20_000 };
        let context = crate::workspace_vcs::context_with_companion(service, request.jj_guest.clone())?;
        let host_context = crate::workspace_vcs::write_host_context(&request.private_root.join("context/launch.json"), &context)?;
        let assignment = WorkspaceAssignment { format_version: 1, fence: request.fence.clone(),
            runnable_root: request.runnable_root.clone(), source: runnable.source.clone(), seed: seed_ref.clone(),
            observation: observation.clone(), host_context: host_context.clone() };
        assignment.validate(&request.fence)?;
        let host_socket = transient.join("backend.sock");
        ensure!(host_socket.as_os_str().as_encoded_bytes().len() < 100, "managed backend socket exceeds portable path bound");
        let guest_socket = PathBuf::from("/tmp/cx.sock");
        let (command, server_command, home_files) = prepare_backend(&request.fence, &request.backend, &request.native_guest, &guest_socket)?;
        let reports = ManagedReportMapping::for_execution(&execution_root.join("runs").join(&request.fence.attempt_id).join("home"));
        let broker = crate::agent_broker::socket_path(&request.home, &request.fence.owner.project_id,
            &request.fence.run_id, &request.fence.attempt_id);
        let mut environment = request.credentials.environment.clone();
        environment.extend([("RUDDER_FABRIC_ATTEMPT_ID".into(), request.fence.attempt_id.clone()),
            ("RUDDER_DONE_FILE".into(), reports.guest_done_file.display().to_string()),
            ("CODEX_HOME".into(), "/home/agent/.codex".into())]);
        let mut authority_roots = vec![private, request.private_root.join("context"), request.private_root.join("state"),
            request.private_root.join("projections"), request.journal.clone()];
        authority_roots.extend(protected_roots);
        authority_roots.push(request.endpoint_parent.clone());
        let spec = ExecutionSpec { profile: None, format_version: 3, execution_id: request.fence.attempt_id.clone(),
            operation_id: request.operation_id.clone(), execution_root, manager_root: manager.root().into(),
            store_root: request.store.root().into(), authority_roots,
            view_id: request.view_id.clone(), owner: request.fence.owner_epoch(), command, environment,
            runtime: request.runtime.clone(), home_files, broker_directory: Some(broker.parent().unwrap().into()),
            broker_binding: Some(BrokerBinding { project_id: request.fence.owner.project_id.clone(),
                run_id: request.fence.run_id.clone(), run_epoch: request.fence.run_epoch, owner_epoch: request.fence.owner.epoch }),
            workspace_observation: Some(WorkspaceObservationProfile { binding: observation, endpoint_directory: endpoint,
                application: Some(HelperApplicationSpec { format_version: 1, kind: crate::workspace_vcs::KIND.into(),
                    host_context, projection_directory: transient.join("vcs") }) }), timeout_seconds: request.timeout_seconds };
        spec.validate()?;
        check()?;
        File::open(&request.private_root)?.sync_all()?;
        File::open(request.private_root.parent().context("managed root parent absent")?)?.sync_all()?;
        Ok(PreparedManagedLaunch { assignment, transport: ManagedTransportMapping { host_socket, guest_socket, guest_cwd: "/workspace".into() }, reports,
            managed_run,
            execution_profile: request.backend.execution_profile().into(), identity_home: request.home.clone(),
            identity_journal: request.journal.clone(), server_command, codex_api_key, spec: Some(spec), retirement: None })
    })();
    match prepared {
        Ok(mut prepared) => { prepared.retirement = Some(retirement); Ok(prepared) }
        Err(error) => { let _ = retire_unused(retirement); Err(error) }
    }
}

fn selected_protected_roots(identity_home: &Path, additional: &[PathBuf]) -> Result<Vec<PathBuf>> {
    ensure!(additional.len() <= 32, "managed protected credential roots exceed bound");
    let roots: Vec<_> = std::iter::once(identity_home.to_path_buf()).chain(additional.iter().cloned()).collect();
    for root in &roots {
        ensure!(root.is_absolute() && root != Path::new("/") && root.components().all(|component|matches!(component,
            std::path::Component::RootDir | std::path::Component::Normal(_))), "managed protected roots must be explicit normalized host paths");
    }
    Ok(roots)
}

fn validate_runtime_program(program: &str, runtime: &[ReadOnlyRuntime]) -> Result<()> {
    let path = Path::new(program);
    ensure!(path.is_absolute() && path.components().all(|component|matches!(component,
        std::path::Component::RootDir | std::path::Component::Normal(_)))
        && runtime.iter().any(|projection|path.starts_with(&projection.guest_path)),
        "managed programs must belong to the declared read-only image runtime");
    Ok(())
}

fn prepare_backend(fence: &ManagedLaunchFence, backend: &ManagedBackendProfile, native_guest: &str,
    socket: &Path) -> Result<(Vec<String>, Option<Vec<String>>, Vec<HomeFile>)> {
    let broker_args = vec!["fabric", "mcp", "--identity-file", "/run/rudder/agent/identity.json", "--broker-socket", "/run/rudder/agent/broker.sock"];
    let config = canonical_json(&serde_json::json!({"mcpServers":{"rudder_agents":{"type":"stdio",
        "command":native_guest,"args":broker_args}}}))?;
    let signal = Path::new("/home/agent/.rudder-reports/signal.json");
    let mut files = vec![HomeFile { relative_path: ".rudder/mcp.json".into(), contents: config },
        HomeFile { relative_path: ".rudder-reports/assignment.json".into(), contents: canonical_json(fence)? }];
    let mut codex_configs = vec!["-c".into(), format!("mcp_servers.rudder_agents.command={}", serde_json::to_string(native_guest)?),
        "-c".into(), format!("mcp_servers.rudder_agents.args={}", serde_json::to_string(&broker_args)?),
        "-c".into(), "mcp_servers.rudder_agents.enabled=true".into(),
        "-c".into(), "notify=[\"/bin/sh\",\"/home/agent/.rudder/notify.sh\"]".into(),
        "-c".into(), "cli_auth_credentials_store=\"ephemeral\"".into()];
    match backend {
        ManagedBackendProfile::CodexAppServer { program, arguments } => {
            ensure!(Path::new(program).is_absolute() && !arguments.iter().any(|a| matches!(a.as_str(), "exec" | "app-server" | "--remote") || a.starts_with("--remote=")), "invalid explicit Codex interactive profile");
            files.push(HomeFile { relative_path: ".rudder/notify.sh".into(), contents: crate::signals::codex_notify_script(signal).into_bytes() });
            files.push(HomeFile { relative_path:".codex/config.toml".into(), contents:b"cli_auth_credentials_store = \"ephemeral\"\n".to_vec() });
            let mut server = vec![program.clone()];
            // The server owns MCP and lifecycle configuration. Preserve explicit
            // root options before applying this attempt's guest-only overrides.
            let mut at = 0;
            while at < arguments.len() {
                let arg = &arguments[at];
                if arg == "--" { break; }
                if matches!(arg.as_str(), "-c" | "--config" | "--enable" | "--disable" | "--profile" | "-p") {
                    let value = arguments.get(at + 1).context("Codex root option is missing its value")?;
                    server.extend([arg.clone(), value.clone()]); at += 2;
                } else if ["--config=", "--enable=", "--disable=", "--profile="].iter().any(|prefix| arg.starts_with(prefix)) {
                    server.push(arg.clone()); at += 1;
                } else { at += 1; }
            }
            server.append(&mut codex_configs);
            server.extend(["app-server".into(), "--listen".into(), format!("unix://{}", socket.display())]);
            let mut terminal = vec![program.clone(), "--remote".into(), format!("unix://{}", socket.display())];
            terminal.extend(arguments.clone());
            let before_prompt = terminal.iter().position(|arg|arg == "--").unwrap_or(terminal.len());
            terminal.splice(before_prompt..before_prompt, ["-c".into(), "cli_auth_credentials_store=\"ephemeral\"".into()]);
            Ok((terminal, Some(server), files))
        }
        ManagedBackendProfile::ClaudeSdk { node, rudder_cli, claude, arguments, initial_prompt } => {
            for path in [node, rudder_cli, claude] { ensure!(Path::new(path).is_absolute(), "managed Claude programs must use explicit guest paths"); }
            files.push(HomeFile { relative_path: ".rudder/claude-settings.json".into(), contents: crate::signals::claude_settings_json(signal, false).into_bytes() });
            let mut arguments = arguments.clone(); arguments.extend(["--mcp-config".into(), "/home/agent/.rudder/mcp.json".into(),
                "--settings".into(), "/home/agent/.rudder/claude-settings.json".into()]);
            files.push(HomeFile { relative_path: ".rudder/claude-launch.json".into(), contents: canonical_json(&serde_json::json!({
                "format_version":1,"run_id":fence.run_id,"attempt_id":fence.attempt_id,"socket_path":socket,
                "claude_program":claude,"claude_args":arguments,"initial_prompt":initial_prompt}))? });
            Ok((vec![node.clone(), rudder_cli.clone(), "__claude-sdk".into(), "--launch-file".into(), "/home/agent/.rudder/claude-launch.json".into()], None, files))
        }
    }
}

#[cfg(test)]
#[path = "managed_launch_tests.rs"]
mod tests;
