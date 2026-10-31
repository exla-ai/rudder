//! Restricted Linux execution over an owned logical workspace.
//!
//! The helper is a fresh process, not the native App loop. Host CAS/journal
//! authority stays outside the agent root. A scoped broker directory may be
//! projected read-only; the helper never opens an agent's journal for MCP.
use crate::{
    protocol::OwnerEpoch,
    vfs::{
        rpc::{AdoptionWitness, AttemptAuthority, ObservationBinding},
        VfsCheckpoint,
    },
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

#[cfg(target_os = "linux")]
pub mod handoff;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
mod root;
#[cfg(target_os = "linux")]
mod state;
pub mod network;
pub mod workspace;
pub use workspace::{
    read_execution_recovery, CheckpointCapability, HelperWorkspaceFactory, HelperWorkspaceResources,
    HistoricalExecution, NewExecution, OwnedHelperWorkspace,
};

/// Caller is the trusted engine ingress, not an agent-supplied mount request.
/// Secrets are never Debug formatted or persisted with the execution receipt.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSpec {
    pub format_version: u32,
    pub execution_id: String,
    pub operation_id: String,
    pub execution_root: PathBuf,
    pub manager_root: PathBuf,
    pub store_root: PathBuf,
    /// Trusted engine supplies every journal, identity/auth and peer credential
    /// root. Public runtime binds must not overlap any of them in either way.
    pub authority_roots: Vec<PathBuf>,
    pub view_id: String,
    pub owner: OwnerEpoch,
    /// Guest executable/arguments, resolved only after restricting the root.
    pub command: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub runtime: Vec<ReadOnlyRuntime>,
    #[serde(default)]
    pub home_files: Vec<HomeFile>,
    /// Dedicated per-attempt directory containing identity.json and an optional
    /// broker.sock. Binding the directory preserves rotation/restart visibility.
    pub broker_directory: Option<PathBuf>,
    pub broker_binding: Option<BrokerBinding>,
    /// Explicit v2 profile. Omitted v1 bytes retain their original canonical
    /// request identity; old helpers reject this unknown field/version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_observation: Option<WorkspaceObservationProfile>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceObservationProfile {
    pub binding: ObservationBinding,
    /// Trusted engine-selected short private projection. Contains only this
    /// workspace identity and socket; no CAS or authority database is bound.
    pub endpoint_directory: PathBuf,
    /// Explicit v3 application. None keeps v1/v2 request bytes unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<HelperApplicationSpec>,
}

/// A protected local file, not an object published into shared CAS. Its exact
/// bytes are bound by the trusted sealed execution specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedHostContextRef {
    pub format_version: u32,
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
}
impl ProtectedHostContextRef {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported protected context reference"
        );
        ensure!(
            self.size_bytes > 0 && self.size_bytes <= 256 * 1024,
            "protected host context exceeds256KiB"
        );
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid protected host context digest"
        );
        safe_absolute(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperApplicationSpec {
    pub format_version: u32,
    pub kind: String,
    pub host_context: ProtectedHostContextRef,
    /// Dedicated private endpoint directory; never a private metadata/CAS root.
    pub projection_directory: PathBuf,
}
impl HelperApplicationSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported helper application specification"
        );
        ensure!(
            !self.kind.is_empty()
                && self.kind.len() <= 48
                && self
                    .kind
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| b.is_ascii_lowercase()
                        || i > 0 && (b.is_ascii_digit() || b == b'-')),
            "invalid helper application kind"
        );
        self.host_context.validate()?;
        safe_absolute(&self.projection_directory)
    }
    pub fn guest_directory(&self) -> String {
        format!("/run/rudder/services/{}", self.kind)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceProjectionKind {
    File { max_size_bytes: u64 },
    Socket,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceProjectionEntry {
    pub name: String,
    pub node: ServiceProjectionKind,
}
/// Constructed by a trusted application only after its owning workspace RPC is
/// ready. This is carried in the helper's sealed child payload, never peer data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestServiceProjection {
    pub directory: PathBuf,
    pub entries: Vec<ServiceProjectionEntry>,
    pub environment: BTreeMap<String, String>,
}
impl GuestServiceProjection {
    pub fn validate(&self, spec: &ExecutionSpec) -> Result<()> {
        let application = spec
            .workspace_observation
            .as_ref()
            .and_then(|w| w.application.as_ref())
            .ok_or_else(|| anyhow::anyhow!("application projection has no sealed assignment"))?;
        ensure!(
            self.directory == application.projection_directory,
            "application projection changed its sealed directory"
        );
        ensure!(
            !self.entries.is_empty() && self.entries.len() <= 8,
            "application projection entry count exceeds bound"
        );
        let mut names = std::collections::BTreeSet::new();
        for entry in &self.entries {
            ensure!(
                !entry.name.is_empty()
                    && entry.name.len() <= 64
                    && !entry.name.starts_with('.')
                    && entry
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                    && names.insert(&entry.name),
                "unsafe or duplicate application projection name"
            );
            if let ServiceProjectionKind::File { max_size_bytes } = entry.node {
                ensure!(
                    max_size_bytes > 0 && max_size_bytes <= 64 * 1024,
                    "application projection file exceeds64KiB"
                );
            }
        }
        ensure!(
            self.environment.len() <= 8,
            "application environment exceeds8 entries"
        );
        for (key, value) in &self.environment {
            ensure!(
                key.starts_with("RUDDER_")
                    && key.len() <= 128
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "application environment key is not namespaced"
            );
            ensure!(
                ![
                    "RUDDER_AGENT_",
                    "RUDDER_WORKSPACE_",
                    "RUDDER_EXECUTION_",
                    "RUDDER_FABRIC_",
                    "RUDDER_ENGINE"
                ]
                .iter()
                .any(|prefix| key.starts_with(prefix)),
                "application cannot override execution authority environment"
            );
            ensure!(
                value.len() <= 4096
                    && !value.contains('\0')
                    && spec
                        .environment
                        .get(key)
                        .is_none_or(|existing| existing == value),
                "application environment conflicts with sealed request"
            );
        }
        Ok(())
    }
}

pub struct WorkspaceRpcReady<'a> {
    pub binding: &'a ObservationBinding,
    pub socket: &'a Path,
    pub identity_file: &'a Path,
    pub application: &'a HelperApplicationSpec,
}
pub trait HelperFinalizationGuard: Send {}
impl<T: Send> HelperFinalizationGuard for T {}
/// Implemented by trusted native code. It is not deserializable or guest
/// selectable. The returned guard must own the actual writer fence until the
/// checkpoint AND execution record are durably published.
pub trait HelperApplication: Send {
    fn start(&mut self, ready: &WorkspaceRpcReady<'_>) -> Result<GuestServiceProjection>;
    fn close_ingress(&self);
    fn quiesce(&mut self) -> Result<Box<dyn HelperFinalizationGuard>>;
}

fn safe_absolute(path: &Path) -> Result<()> {
    use std::path::Component;
    ensure!(
        path.is_absolute()
            && path != Path::new("/")
            && !path.as_os_str().as_encoded_bytes().contains(&0)
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "unsafe protected application path"
    );
    Ok(())
}

pub const WORKSPACE_GUEST_DIRECTORY: &str = "/run/rudder/workspace";
pub const WORKSPACE_GUEST_SOCKET: &str = "/run/rudder/workspace/workspace.sock";
pub const WORKSPACE_GUEST_IDENTITY: &str = "/run/rudder/workspace/identity.json";

pub struct HelperServices {
    pub workspace_authority: Arc<dyn AttemptAuthority>,
    pub adoption_witness: Option<Arc<dyn AdoptionWitness>>,
    pub application: Option<Box<dyn HelperApplication>>,
    /// Invoked only for a new, locked execution after services authorization.
    /// None preserves the existing complete local manager/open_view path.
    pub workspace_factory: Option<HelperWorkspaceFactory>,
}
/// Invoked only AFTER fresh single-thread namespace entry. Constructing this
/// closure must not spawn threads; neither may sealed-spec handoff preparation.
pub type HelperServicesFactory = Box<dyn FnOnce(&ExecutionSpec) -> Result<HelperServices>>;

/// Credential-free binding carried alongside the sealed spec. Legacy specs and
/// handoffs retain their exact encoding; this requires managed-helper dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedFrontendStart { Immediate, AfterHostApiKeyV1 }

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedExecutionBinding {
    pub format_version: u32,
    pub request_id: String,
    pub run_id: String,
    pub run_epoch: u64,
    pub execution_id: String,
    pub operation_id: String,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub execution_profile: String,
    pub frontend_start: ManagedFrontendStart,
    pub server_command: Option<Vec<String>>,
    pub guest_socket: PathBuf,
}
impl ManagedExecutionBinding {
    pub fn validate(&self, spec: &ExecutionSpec) -> Result<()> {
        for id in [&self.request_id, &self.execution_id, &self.operation_id, &self.view_id] {
            ensure!(Uuid::parse_str(id).is_ok_and(|u| u.to_string() == *id), "noncanonical managed execution identity");
        }
        ensure!(self.format_version == 2 && self.execution_id == spec.execution_id
            && self.operation_id == spec.operation_id && self.view_id == spec.view_id && self.owner == spec.owner
            && self.run_epoch > 0 && !self.run_id.is_empty() && self.run_id.len() <= 256
            && matches!(self.execution_profile.as_str(), "codex-app-server" | "claude-sdk")
            && self.guest_socket == Path::new("/tmp/cx.sock") && spec.format_version == 3,
            "managed execution differs from the reserved local assignment");
        let broker = spec.broker_binding.as_ref().ok_or_else(||anyhow::anyhow!("managed execution has no broker binding"))?;
        ensure!(broker.run_id == self.run_id && broker.run_epoch == self.run_epoch
            && broker.owner_epoch == self.owner.epoch, "managed broker fence differs");
        ensure!((self.execution_profile == "codex-app-server") == self.server_command.is_some(),
            "managed server/profile mismatch");
        ensure!(self.frontend_start == if self.execution_profile == "codex-app-server" { ManagedFrontendStart::AfterHostApiKeyV1 }
            else { ManagedFrontendStart::Immediate }, "managed frontend has no matching authenticated-start protocol");
        if let Some(command) = &self.server_command {
            ensure!(!command.is_empty() && command.len() <= 256 && Path::new(&command[0]).is_absolute()
                && command.iter().all(|a| a.len() <= 64 * 1024 && !a.contains('\0')),
                "invalid managed backend command");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopAndCheckpoint {
    pub request_id: String,
    pub execution_id: String,
    pub operation_id: String,
    pub view_id: String,
    pub run_id: String,
    pub run_epoch: u64,
    pub owner: OwnerEpoch,
}
impl StopAndCheckpoint {
    pub fn validate(&self, binding: &ManagedExecutionBinding) -> Result<()> {
        ensure!(Uuid::parse_str(&self.request_id).is_ok_and(|u|u.to_string() == self.request_id)
            && self.execution_id == binding.execution_id && self.operation_id == binding.operation_id
            && self.view_id == binding.view_id && self.run_id == binding.run_id
            && self.run_epoch == binding.run_epoch && self.owner == binding.owner,
            "shutdown request differs from the owned managed attempt");
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedShutdownReceipt {
    pub request: StopAndCheckpoint,
    pub record: ExecutionRecord,
    pub kernel_drained: bool,
}

pub fn workspace_scope(binding: &ObservationBinding) -> Result<String> {
    use sha2::{Digest, Sha256};
    binding.validate()?;
    Ok(format!(
        "workspace:{}",
        hex::encode(Sha256::digest(crate::protocol::canonical_json(binding)?))
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadOnlyRuntime {
    pub source: PathBuf,
    pub guest_path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerBinding {
    pub project_id: String,
    pub run_id: String,
    pub run_epoch: u64,
    pub owner_epoch: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeFile {
    pub relative_path: String,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    Preparing,
    Running,
    Checkpointed,
    Orphaned,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRecord {
    pub format_version: u32,
    pub execution_id: String,
    pub operation_id: String,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub request_digest: String,
    pub phase: ExecutionPhase,
    pub supervisor_pid: u32,
    pub agent_exit: Option<i32>,
    pub checkpoint: Option<VfsCheckpoint>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionOutcome {
    pub record: ExecutionRecord,
    pub isolation: ExecutionIsolation,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionIsolation {
    pub same_uid_user_namespace: bool,
    pub private_mount_namespace: bool,
    pub kernel_pid_namespace_drained: bool,
    pub restricted_root: bool,
    pub read_only_runtime: bool,
    pub inherited_authority_fds_closed: bool,
    pub no_new_privileges: bool,
    pub network_isolated: bool,
    pub cgroup_limited: bool,
}
impl ExecutionSpec {
    pub fn validate(&self) -> Result<()> {
        let application = self
            .workspace_observation
            .as_ref()
            .and_then(|w| w.application.as_ref());
        ensure!(
            (self.format_version == 1 && self.workspace_observation.is_none())
                || (self.format_version == 2
                    && self.workspace_observation.is_some()
                    && application.is_none())
                || (self.format_version == 3 && application.is_some()),
            "unsupported execution specification"
        );
        for id in [&self.execution_id, &self.operation_id, &self.view_id] {
            ensure!(
                Uuid::parse_str(id).is_ok(),
                "execution identifiers must be UUIDs"
            );
        }
        self.owner.validate()?;
        if let Some(workspace) = &self.workspace_observation {
            if let Some(application) = &workspace.application {
                application.validate()?;
            }
            workspace.binding.validate()?;
            ensure!(
                workspace.binding.attempt.execution_id == self.execution_id
                    && workspace.binding.attempt.operation_id == self.operation_id
                    && workspace.binding.view_id == self.view_id
                    && workspace.binding.owner == self.owner,
                "workspace observation binding differs from execution"
            );
            ensure!(
                self.broker_directory.is_some() && self.broker_binding.is_some(),
                "workspace observations require a current scoped agent broker"
            );
            ensure!(
                workspace.endpoint_directory.is_absolute()
                    && workspace.endpoint_directory.components().all(|c| matches!(
                        c,
                        std::path::Component::RootDir | std::path::Component::Normal(_)
                    ))
                    && workspace
                        .endpoint_directory
                        .join("workspace.sock")
                        .as_os_str()
                        .as_encoded_bytes()
                        .len()
                        < 100,
                "workspace endpoint requires a short normalized absolute path"
            );
        }
        ensure!(
            self.broker_directory.is_some() == self.broker_binding.is_some(),
            "broker projection requires its explicit attempt binding"
        );
        if let Some(binding) = &self.broker_binding {
            ensure!(
                !binding.project_id.is_empty()
                    && !binding.run_id.is_empty()
                    && binding.run_epoch > 0
                    && binding.owner_epoch == self.owner.epoch,
                "broker binding differs from assigned execution owner"
            );
        }
        ensure!(
            self.execution_root.is_absolute()
                && self.manager_root.is_absolute()
                && self.store_root.is_absolute(),
            "execution paths must be absolute"
        );
        ensure!(
            !self.authority_roots.is_empty()
                && self.authority_roots.len() <= 64
                && self.authority_roots.iter().all(|p| p.is_absolute()),
            "execution authority roots must be explicitly declared"
        );
        ensure!(
            !self.command.is_empty()
                && self.command.len() <= 256
                && self
                    .command
                    .iter()
                    .all(|s| !s.contains('\0') && s.len() <= 64 * 1024),
            "invalid execution command"
        );
        ensure!(
            self.timeout_seconds > 0 && self.timeout_seconds <= 24 * 60 * 60,
            "execution deadline must be1..86400seconds"
        );
        ensure!(
            self.runtime.len() <= 64
                && self.home_files.len() <= 128
                && self.environment.len() <= 128,
            "execution projection count exceeds limit"
        );
        ensure!(
            self.home_files
                .iter()
                .map(|f| f.contents.len())
                .sum::<usize>()
                <= 512 * 1024,
            "private home seeds exceed512KiB"
        );
        ensure!(
            self.environment.iter().all(|(k, v)| !k.is_empty()
                && k.len() <= 128
                && k.bytes().enumerate().all(|(i, b)| b.is_ascii_alphabetic()
                    || b == b'_'
                    || i > 0 && b.is_ascii_digit())
                && !v.contains('\0')
                && v.len() <= 64 * 1024),
            "invalid execution environment"
        );
        #[cfg(target_os = "linux")]
        root::validate(self)?;
        Ok(())
    }
}

pub fn capabilities() -> serde_json::Value {
    serde_json::json!({
        "format_version":1,"platform":"linux","requires":"fresh UID1500 helper + boot-granted /dev/fuse access",
        "authority":"trusted engine spec, per-view owner epoch; no journal mounted",
        "execution_spec_versions":[1,2,3],"workspace_observation":"explicit v2 profile; v3 adds protected application context; qualified broker authorization required; default off",
        "network_isolated":false,"cgroup_limited":false,"macos_default_changed":false
    })
}

pub fn run_helper(spec: ExecutionSpec) -> Result<ExecutionOutcome> {
    spec.validate()?;
    ensure!(
        spec.workspace_observation.is_none(),
        "workspace observation requires a qualified live authority factory"
    );
    #[cfg(target_os = "linux")]
    {
        linux::run_helper(spec)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = spec;
        anyhow::bail!("restricted execution is supported only on qualified Linux workers")
    }
}

pub fn run_helper_with_services(
    spec: ExecutionSpec,
    factory: HelperServicesFactory,
) -> Result<ExecutionOutcome> {
    spec.validate()?;
    #[cfg(target_os = "linux")]
    {
        linux::run_helper_with_services(spec, factory)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (spec, factory);
        anyhow::bail!("restricted execution is supported only on qualified Linux workers")
    }
}
