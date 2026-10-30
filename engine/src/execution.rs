//! Restricted Linux execution over an owned logical workspace.
//!
//! The helper is a fresh process, not the native App loop. Host CAS/journal
//! authority stays outside the agent root. A scoped broker directory may be
//! projected read-only; the helper never opens an agent's journal for MCP.
use crate::{
    protocol::{OwnerEpoch, FORMAT_VERSION},
    vfs::VfsCheckpoint,
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use uuid::Uuid;

#[cfg(target_os = "linux")]
pub mod handoff;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
mod root;
#[cfg(target_os = "linux")]
mod state;

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
    pub timeout_seconds: u64,
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
        ensure!(
            self.format_version == FORMAT_VERSION,
            "unsupported execution specification"
        );
        for id in [&self.execution_id, &self.operation_id, &self.view_id] {
            ensure!(
                Uuid::parse_str(id).is_ok(),
                "execution identifiers must be UUIDs"
            );
        }
        self.owner.validate()?;
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
        "network_isolated":false,"cgroup_limited":false,"macos_default_changed":false
    })
}

pub fn run_helper(spec: ExecutionSpec) -> Result<ExecutionOutcome> {
    spec.validate()?;
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
