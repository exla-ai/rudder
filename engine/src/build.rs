//! Prepared environments and frontend-validated build results are different
//! indexes. Rudder metadata digests are not REAPI protobuf action digests.
//!
//! The first managed profile generates trusted Buck2 compile rules. It does
//! not accept arbitrary shell commands or a peer's assertion of hermeticity.
//! Native Buck2 owns action validity and REAPI cache lookups; this module keeps
//! immutable roots, admission provenance and the explicit project contract.

use crate::protocol::{canonical_json, ObjectKind, ObjectRef, FORMAT_VERSION};
use crate::store::{LocalStore, ReferencedMetadata};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

mod frontend;
mod index;
pub use frontend::{Buck2Adapter, BuildRun};
pub use index::{BuildIndex, PreparedEnvironment, StoredBuild};

/// Produced by workspace::build_input_view after validating a complete sealed
/// checkpoint and its source/prepared partition. No peer deserializer or bare
/// source/base-environment fallback can assert this proof.
#[derive(Debug, Clone, Serialize)]
pub struct BuildInputView {
    checkpoint: ObjectRef,
    source_root: ObjectRef,
    view_root: ObjectRef,
    base_environment: Option<ObjectRef>,
    profile: ObjectRef,
    hardlinks: Option<ObjectRef>,
    hardlink_group_count: usize,
    effective_prepared_fingerprint: String,
    base_prepared_fingerprint: String,
}
impl BuildInputView {
    pub(crate) fn from_validated_checkpoint(
        checkpoint: ObjectRef,
        source_root: ObjectRef,
        view_root: ObjectRef,
        base_environment: Option<ObjectRef>,
        profile: ObjectRef,
        hardlinks: Option<ObjectRef>,
        hardlink_group_count: usize,
        effective_prepared_fingerprint: String,
        base_prepared_fingerprint: String,
    ) -> Self {
        Self {
            checkpoint,
            source_root,
            view_root,
            base_environment,
            profile,
            hardlinks,
            hardlink_group_count,
            effective_prepared_fingerprint,
            base_prepared_fingerprint,
        }
    }
    pub fn checkpoint(&self) -> &ObjectRef {
        &self.checkpoint
    }
    pub fn source_root(&self) -> &ObjectRef {
        &self.source_root
    }
    pub fn view_root(&self) -> &ObjectRef {
        &self.view_root
    }
    pub fn base_environment(&self) -> Option<&ObjectRef> {
        self.base_environment.as_ref()
    }
    pub fn profile(&self) -> &ObjectRef {
        &self.profile
    }
    pub fn hardlinks(&self) -> Option<&ObjectRef> {
        self.hardlinks.as_ref()
    }
    pub fn hardlink_group_count(&self) -> usize {
        self.hardlink_group_count
    }
    pub fn effective_prepared_fingerprint(&self) -> &str {
        &self.effective_prepared_fingerprint
    }
    pub fn base_prepared_fingerprint(&self) -> &str {
        &self.base_prepared_fingerprint
    }
    pub fn dirty_prepared(&self) -> bool {
        self.effective_prepared_fingerprint != self.base_prepared_fingerprint
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformIdentity {
    pub os: String,
    pub architecture: String,
    pub abi: String,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}
impl PlatformIdentity {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.os.is_empty() && !self.architecture.is_empty() && !self.abi.is_empty(),
            "platform OS, architecture and ABI are required"
        );
        Ok(())
    }
    pub fn properties(&self) -> BTreeMap<String, String> {
        let mut values = self.properties.clone();
        values.insert("os".into(), self.os.clone());
        values.insert("architecture".into(), self.architecture.clone());
        values.insert("abi".into(), self.abi.clone());
        values
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainIdentity {
    pub version: String,
    /// Complete compiler/runtime inputs, including SDK and standard libraries.
    pub closure: ObjectRef,
    pub executable: ObjectRef,
    /// Logical executable inside the prepared tool filesystem.
    pub executable_path: String,
    #[serde(default)]
    pub configuration: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationSpec {
    pub format_version: u32,
    pub platform: PlatformIdentity,
    pub base_runtime: ObjectRef,
    /// Lockfiles, installer scripts, patches, native package configuration.
    /// Source files belong in ActionContract instead of this map.
    pub installer_inputs: BTreeMap<String, ObjectRef>,
    pub installer_commands: Vec<Vec<String>>,
    pub installer_environment: BTreeMap<String, String>,
    pub toolchains: BTreeMap<String, ToolchainIdentity>,
    pub policy_version: String,
    /// Secret/unmodeled acquisition happens before immutable installer inputs
    /// enter this index. Such effects cannot publish a reusable environment.
    #[serde(default)]
    pub effects: BTreeSet<BuildEffect>,
}
impl PreparationSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == FORMAT_VERSION,
            "unsupported preparation format"
        );
        self.platform.validate()?;
        kind(&self.base_runtime, ObjectKind::Directory)?;
        ensure!(
            !self.policy_version.is_empty() && !self.toolchains.is_empty(),
            "preparation needs policy and toolchain identities"
        );
        ensure!(
            self.effects.is_empty(),
            "effectful preparation is not reusable"
        );
        for (name, tool) in &self.toolchains {
            ensure!(
                !name.is_empty() && !tool.version.is_empty(),
                "toolchain name/version is required"
            );
            kind(&tool.closure, ObjectKind::Directory)?;
            kind(&tool.executable, ObjectKind::Blob)?;
            relative(&tool.executable_path)?;
        }
        for (path, reference) in &self.installer_inputs {
            relative(path)?;
            reference.validate()?;
        }
        safe_environment(&self.installer_environment)?;
        ensure!(
            self.installer_commands
                .iter()
                .all(|argv| !argv.is_empty() && argv.iter().all(|arg| !arg.contains('\0'))),
            "invalid installer command"
        );
        Ok(())
    }
    pub fn key(&self, store: &LocalStore) -> Result<ObjectRef> {
        self.validate()?;
        let mut references = vec![self.base_runtime.clone()];
        references.extend(self.installer_inputs.values().cloned());
        for tool in self.toolchains.values() {
            references.extend([tool.closure.clone(), tool.executable.clone()]);
        }
        metadata(
            store,
            ObjectKind::ActionKey,
            "prepared-environment-key-v1",
            self,
            references,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildEffect {
    Network,
    Secrets,
    Signing,
    Deployment,
    ExternalState,
    Clock,
    Randomness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildPolicy {
    pub version: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub effects: BTreeSet<BuildEffect>,
}
impl BuildPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.version.is_empty(), "build policy version is required");
        safe_environment(&self.environment)?;
        for (name, expected) in [("LANG", "C"), ("LC_ALL", "C"), ("TZ", "UTC")] {
            ensure!(
                self.environment
                    .get(name)
                    .is_none_or(|value| value == expected),
                "managed compiler locale/timezone is fixed: {name}"
            );
        }
        Ok(())
    }
    pub fn cacheable(&self) -> bool {
        self.effects.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompileLanguage {
    Rust,
    #[serde(rename = "typescript")]
    TypeScript,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompileTarget {
    pub name: String,
    pub language: CompileLanguage,
    pub entrypoint: String,
    pub sources: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteCacheConfig {
    pub cas_address: String,
    pub action_cache_address: String,
    pub instance_name: String,
    pub tls: bool,
}
impl RemoteCacheConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.cas_address.is_empty() && !self.action_cache_address.is_empty(),
            "REAPI cache endpoints are required"
        );
        for value in [
            &self.cas_address,
            &self.action_cache_address,
            &self.instance_name,
        ] {
            ensure!(
                !value.contains(['\n', '\r', '\0']),
                "invalid REAPI configuration value"
            );
        }
        Ok(())
    }
    /// Buck2's own gRPC REAPI client handles protobuf AC/CAS. This is not the
    /// Rudder REST CAS protocol and does not translate Rudder metadata keys.
    pub fn buckconfig(&self) -> Result<String> {
        self.validate()?;
        Ok(format!("[buck2_re_client]\ncas_address = {}\naction_cache_address = {}\nengine_address = {}\ninstance_name = {}\ntls = {}\n",self.cas_address,self.action_cache_address,self.action_cache_address,self.instance_name,self.tls))
    }
    pub fn bazel_flags(&self) -> Result<Vec<String>> {
        self.validate()?;
        ensure!(
            self.cas_address == self.action_cache_address,
            "Bazel's remote_cache flag requires a combined endpoint"
        );
        Ok(vec![
            format!("--remote_cache={}", self.action_cache_address),
            format!("--remote_instance_name={}", self.instance_name),
            "--remote_accept_cached=true".into(),
            "--remote_upload_local_results=true".into(),
        ])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Buck2Profile {
    pub release: String,
    pub executable_sha256: String,
    #[serde(default)]
    pub remote_cache: Option<RemoteCacheConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildProfile {
    pub format_version: u32,
    pub name: String,
    pub platform: PlatformIdentity,
    pub buck2: Buck2Profile,
    pub policy: BuildPolicy,
    pub targets: Vec<CompileTarget>,
}
impl BuildProfile {
    pub fn from_toml(text: &str) -> Result<Self> {
        let profile: Self =
            toml::from_str(text).context("parse tracked rudder.toml build profile")?;
        profile.validate()?;
        Ok(profile)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == FORMAT_VERSION,
            "unsupported build profile format"
        );
        self.platform.validate()?;
        self.policy.validate()?;
        ensure!(
            self.platform.os == "linux",
            "managed compile profile currently requires Linux namespaces"
        );
        ensure!(
            !self.name.is_empty() && !self.buck2.release.is_empty(),
            "profile name and Buck2 release are required"
        );
        digest(&self.buck2.executable_sha256)?;
        ensure!(
            !self.targets.is_empty() && self.targets.len() <= 256,
            "build profile must have 1..256 targets"
        );
        let mut names = BTreeSet::new();
        for target in &self.targets {
            ensure!(
                !target.name.is_empty()
                    && target
                        .name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    && names.insert(&target.name),
                "invalid or duplicate target name"
            );
            relative(&target.entrypoint)?;
            ensure!(
                target.sources.contains(&target.entrypoint),
                "entrypoint must be a declared source"
            );
            ensure!(!target.sources.is_empty(), "target source closure is empty");
            for source in &target.sources {
                relative(source)?;
            }
        }
        if let Some(cache) = &self.buck2.remote_cache {
            cache.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionContract {
    pub format_version: u32,
    pub frontend: String,
    pub frontend_executable: ObjectRef,
    pub rules: ObjectRef,
    pub source_root: ObjectRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_topology: Option<ObjectRef>,
    pub environment_root: ObjectRef,
    pub platform: PlatformIdentity,
    pub toolchains: BTreeMap<String, ToolchainIdentity>,
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub outputs: BTreeSet<String>,
    pub policy: BuildPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReapiDigest {
    pub hash: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendObservation {
    pub frontend: String,
    pub trace_id: String,
    pub executed_actions: u64,
    pub remote_cache_hits: u64,
    pub native_action_digests: Vec<ReapiDigest>,
    pub event_log: ObjectRef,
    pub build_report: ObjectRef,
}

/// Only the managed frontend adapter constructs this admission capability.
/// No Deserialize implementation allows an agent to assert qualified effects.
#[derive(Debug)]
pub struct QualifiedBuild {
    contract: ActionContract,
    prepared: PreparedEnvironment,
    outputs: ObjectRef,
    observation: FrontendObservation,
}
impl QualifiedBuild {
    pub fn contract(&self) -> &ActionContract {
        &self.contract
    }
    pub fn outputs(&self) -> &ObjectRef {
        &self.outputs
    }
    pub fn observation(&self) -> &FrontendObservation {
        &self.observation
    }
}
impl ActionContract {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == FORMAT_VERSION,
            "unsupported action contract format"
        );
        ensure!(
            self.frontend == "buck2-rudder-linux-compile-v1",
            "unqualified build frontend"
        );
        kind(&self.frontend_executable, ObjectKind::Blob)?;
        kind(&self.rules, ObjectKind::Blob)?;
        kind(&self.source_root, ObjectKind::SourceRoot)?;
        if let Some(topology) = &self.input_topology {
            kind(topology, ObjectKind::Blob)?;
        }
        kind(&self.environment_root, ObjectKind::EnvironmentRoot)?;
        self.platform.validate()?;
        self.policy.validate()?;
        safe_environment(&self.environment)?;
        ensure!(
            !self.command.is_empty() && !self.outputs.is_empty() && !self.toolchains.is_empty(),
            "action command, outputs and toolchain closure are required"
        );
        for output in &self.outputs {
            relative(output)?;
        }
        for tool in self.toolchains.values() {
            kind(&tool.closure, ObjectKind::Directory)?;
            kind(&tool.executable, ObjectKind::Blob)?;
            relative(&tool.executable_path)?;
        }
        Ok(())
    }
    pub fn key(&self, store: &LocalStore) -> Result<ObjectRef> {
        self.validate()?;
        let mut refs = vec![
            self.frontend_executable.clone(),
            self.rules.clone(),
            self.source_root.clone(),
            self.environment_root.clone(),
        ];
        if let Some(topology) = &self.input_topology {
            refs.push(topology.clone());
        }
        for tool in self.toolchains.values() {
            refs.extend([tool.closure.clone(), tool.executable.clone()]);
        }
        metadata(
            store,
            ObjectKind::ActionKey,
            "frontend-action-contract-v1",
            self,
            refs,
        )
    }
}

fn metadata<T: Serialize>(
    store: &LocalStore,
    kind: ObjectKind,
    schema: &str,
    value: &T,
    mut references: Vec<ObjectRef>,
) -> Result<ObjectRef> {
    references.sort();
    references.dedup();
    store.put_metadata(
        kind,
        &ReferencedMetadata {
            format_version: FORMAT_VERSION,
            references,
            payload: serde_json::json!({"schema":schema,"value":value}),
        },
    )
}
fn kind(reference: &ObjectRef, expected: ObjectKind) -> Result<()> {
    reference.validate()?;
    ensure!(reference.kind == expected, "incorrect build object kind");
    Ok(())
}
fn digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid SHA-256 digest"
    );
    Ok(())
}
fn relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains(['\0', '\\'])
            && Path::new(path)
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "path must be normalized and relative"
    );
    Ok(())
}
fn safe_environment(values: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in values {
        ensure!(
            matches!(key.as_str(), "LANG" | "LC_ALL" | "TZ" | "SOURCE_DATE_EPOCH")
                || key.starts_with("RUDDER_BUILD_"),
            "environment variable is outside the managed compiler allowlist"
        );
        ensure!(!value.contains('\0'), "invalid environment value");
    }
    Ok(())
}
fn decode_payload<T: serde::de::DeserializeOwned>(
    store: &LocalStore,
    reference: &ObjectRef,
    schema: &str,
) -> Result<T> {
    let metadata: ReferencedMetadata = serde_json::from_slice(&store.read_object(reference)?)?;
    ensure!(
        metadata.format_version == FORMAT_VERSION && metadata.payload["schema"] == schema,
        "invalid build metadata schema"
    );
    Ok(serde_json::from_value(
        metadata
            .payload
            .get("value")
            .context("missing build metadata value")?
            .clone(),
    )?)
}
