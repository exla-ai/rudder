//! Managed private views, retained immutable roots, and terminal checkpoints.
//!
//! prepare() performs the expensive validation/materialization once. create()
//! only creates descriptors/upper/work directories and forks existing pins; its
//! cost is independent of inherited file count, but retention JSON commits scale
//! with retained pin count. Eager/APFS materialization is explicitly separate.
//! Linux mounts run ONLY in a dedicated fresh helper, never in the engine/UI.
//! Checkpointing reconciles the whole quiesced visible tree, not a cheap delta.

use crate::protocol::{
    canonical_json, CheckpointConsistency, Durability, ObjectKind, ObjectRef, OwnerEpoch,
    FORMAT_VERSION,
};
use crate::store::{
    CheckpointV2Manifest, CheckpointV3Manifest, DirectoryEntry, EntryNode, EnvironmentManifest, ImportPolicy, LocalStore,
    SourceManifest,
};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    fs::{OpenOptionsExt, PermissionsExt},
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

#[cfg(target_os = "linux")]
pub mod linux;
pub mod materialize;

pub type RawPath = Vec<Vec<u8>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceProfile {
    pub format_version: u32,
    pub prepared: Vec<RawPath>,
    pub scratch: Vec<RawPath>,
    pub vcs_local: Vec<RawPath>,
}

impl WorkspaceProfile {
    pub fn source_only() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            ..Self::default()
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == FORMAT_VERSION,
            "unsupported workspace profile"
        );
        let paths: Vec<_> = self
            .prepared
            .iter()
            .chain(&self.scratch)
            .chain(&self.vcs_local)
            .collect();
        for (index, path) in paths.iter().enumerate() {
            ensure!(
                !path.is_empty(),
                "a partition cannot claim the entire source root"
            );
            for component in path.iter() {
                validate_name(component)?;
            }
            for other in &paths[..index] {
                ensure!(
                    !path.starts_with(other) && !other.starts_with(path),
                    "workspace partitions overlap"
                );
            }
        }
        Ok(())
    }
    pub fn source_exclusions(&self) -> ImportPolicy {
        ImportPolicy {
            excluded_paths: self
                .prepared
                .iter()
                .chain(&self.scratch)
                .chain(&self.vcs_local)
                .cloned()
                .collect(),
        }
    }
}
impl Default for WorkspaceProfile {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            prepared: vec![],
            scratch: vec![],
            vcs_local: vec![],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceBackend {
    LinuxOverlay,
    MacApfsClone,
    EagerCopy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareRequest {
    pub source: ObjectRef,
    pub environment: Option<ObjectRef>,
    pub profile: WorkspaceProfile,
    pub backend: WorkspaceBackend,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedWorkspace {
    pub format_version: u32,
    pub prepared_id: String,
    pub source: ObjectRef,
    pub environment: Option<ObjectRef>,
    pub profile: ObjectRef,
    pub combined_root: ObjectRef,
    pub execution_root: ObjectRef,
    pub backend: WorkspaceBackend,
    pub lower: PathBuf,
    /// Highest-precedence source layer followed by prepared environment layers.
    pub lower_layers: Vec<PathBuf>,
    pub vcs_remint_required: bool,
    pub origin_checkpoint: Option<ObjectRef>,
    pub root_mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    pub view_id: String,
    pub operation_id: String,
    pub prepared_id: String,
    /// Supplied only by the trusted engine after journal authority validation.
    pub owner: OwnerEpoch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    Preparing,
    Created,
    Materialized,
    Running,
    Quiesced,
    Orphaned,
    Sealed,
    Destroyed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDescriptor {
    pub format_version: u32,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub revision: u64,
    pub creation_digest: String,
    pub prepared_id: String,
    pub status: WorkspaceStatus,
    pub active_helper: Option<String>,
    pub latest_checkpoint: Option<ObjectRef>,
    pub operations: BTreeMap<String, WorkspaceReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceReceipt {
    pub operation_id: String,
    pub operation: String,
    pub checkpoint: Option<ObjectRef>,
    pub revision: u64,
    pub knowledge_cursor: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializationReport {
    pub backend: WorkspaceBackend,
    pub path: PathBuf,
    pub inherited_bytes_copied: bool,
    pub entry_count_work: bool,
    pub vcs_remint_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHelperSpec {
    pub manager_root: PathBuf,
    pub store_root: PathBuf,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub command: Vec<String>,
    /// Trusted bridge creates fresh Git/jj metadata after old metadata is removed.
    pub vcs_remint_command: Option<Vec<String>>,
    /// Explicit filesystem-only operation; never a silent VCS compatibility claim.
    pub allow_unversioned: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceManager {
    root: PathBuf,
    store: LocalStore,
}

/// Static requirements, not a substitute for fresh-helper/selected-volume
/// qualification. A successful Linux HelperOutcome contains the real result.
pub fn capabilities() -> serde_json::Value {
    serde_json::json!({
        "format_version":1,
        "create":"retained-root pins plus private descriptor/upper/work metadata",
        "create_cost":"independent of inherited source bytes/entries; retention-record-count cost remains",
        "linux_overlay":{
            "compiled":cfg!(target_os="linux"),"runtime_qualification_required":true,
            "qualified_filesystem":"ext4","mount_scope":"fresh private user/mount/PID namespace helper",
            "prepared_layers":"independent source/environment lower trees retained by digest",
            "copy_up_granularity":"file","lower_directory_rename":false,"hardlink_identity":false,
            "live_checkpoint":false,"terminal_checkpoint":"namespace init fully exited, then syncfs",
            "forced_owner_crash_recovery":"fail closed until explicit helper-completion proof"
        },
        "mac_apfs_clone":{
            "compiled":cfg!(target_os="macos"),"runtime_volume_qualification_required":true,
            "data_clone":"forced clonefile; no byte-copy fallback","visits_directory_entries":true
        },
        "eager_copy":{"compiled":cfg!(unix),"copies_inherited_bytes":true,"visits_directory_entries":true},
        "checkpoint_cost":"full visible-tree reconciliation; not an incremental upper-delta checkpoint",
        "caller_quiesced_api":"explicit caller assertion only; not a process-writer barrier",
        "vcs_restore":"recovery bytes retained; runnable metadata stripped and must be reminted by trusted bridge",
        "replication":false,"security_sandbox":false
    })
}

/// Derive build identity from an actual checkpoint-v2, never caller-asserted
/// source/base-environment pointers. The execution owner retains the checkpoint
/// for the build lifetime. Only changed path metadata is inspected; this helper
/// does not copy or mutate workspace files or silently bless dirty dependencies.
pub fn build_input_view(
    store: &LocalStore,
    checkpoint: &ObjectRef,
) -> Result<crate::build::BuildInputView> {
    ensure!(
        checkpoint.kind == ObjectKind::Checkpoint,
        "build input requires a checkpoint"
    );
    let bytes = store.read_object(checkpoint)?;
    let format = serde_json::from_slice::<serde_json::Value>(&bytes)?["format_version"].as_u64();
    let (value, hardlinks, hardlink_group_count) = match format {
        Some(2) => (serde_json::from_slice::<CheckpointV2Manifest>(&bytes)?, None, 0),
        Some(3) => {
            let value: CheckpointV3Manifest = serde_json::from_slice(&bytes)?;
            let topology = crate::vfs::validate_snapshot(store, &crate::vfs::VfsCheckpoint {
                format_version: 1, directory: value.view_root.clone(), hardlinks: value.hardlinks.clone(),
            })?;
            let normalized = CheckpointV2Manifest {
                format_version: 2, source: value.source, environment: value.environment,
                view_root: value.view_root, profile: value.profile, parents: value.parents,
                knowledge_cursor: value.knowledge_cursor,
                consistency: value.consistency, durability: value.durability,
            };
            (normalized, Some(value.hardlinks), topology.groups.len())
        }
        _ => bail!("managed build identity requires checkpoint schema 2 or 3"),
    };
    let profile: WorkspaceProfile = serde_json::from_slice(&store.read_object(&value.profile)?)?;
    profile.validate()?;
    let source: SourceManifest = serde_json::from_slice(&store.read_object(&value.source)?)?;
    let source_partition = derive_source_partition(
        store,
        &value.view_root,
        &mut Vec::new(),
        &profile.source_exclusions().excluded_paths,
    )?;
    ensure!(
        source_partition == source.directory,
        "checkpoint source root does not match the full-view source partition"
    );
    let effective = prepared_fingerprint(store, Some(&value.view_root), &profile.prepared)?;
    let base_directory = if let Some(environment) = &value.environment {
        let environment: EnvironmentManifest =
            serde_json::from_slice(&store.read_object(environment)?)?;
        Some(environment.directory)
    } else {
        None
    };
    let base = prepared_fingerprint(store, base_directory.as_ref(), &profile.prepared)?;
    Ok(crate::build::BuildInputView::from_validated_checkpoint(
        checkpoint.clone(),
        value.source,
        value.view_root,
        value.environment,
        value.profile,
        hardlinks,
        hardlink_group_count,
        effective,
        base,
    ))
}

fn derive_source_partition(
    store: &LocalStore,
    root: &ObjectRef,
    path: &mut RawPath,
    excluded: &[RawPath],
) -> Result<ObjectRef> {
    let mut directory = store.read_directory(root)?;
    let mut entries = Vec::new();
    for mut entry in directory.entries {
        path.push(entry.name()?);
        if excluded.iter().any(|prefix| path == prefix) {
            path.pop();
            continue;
        }
        if excluded.iter().any(|prefix| prefix.starts_with(path)) {
            if let EntryNode::Directory { object } = &entry.node {
                entry.node = EntryNode::Directory {
                    object: derive_source_partition(store, object, path, excluded)?,
                };
            }
        }
        path.pop();
        entries.push(entry);
    }
    directory.entries = entries;
    Ok(ObjectRef::from_bytes(
        ObjectKind::Directory,
        &canonical_json(&directory)?,
    ))
}

fn prepared_fingerprint(
    store: &LocalStore,
    root: Option<&ObjectRef>,
    prefixes: &[RawPath],
) -> Result<String> {
    let mut selected = Vec::new();
    for prefix in prefixes {
        let entry = if let Some(root) = root {
            lookup_entry(store, root, prefix)?.map(|entry| entry.node)
        } else {
            None
        };
        selected.push((prefix, entry));
    }
    Ok(ObjectRef::from_bytes(
        ObjectKind::Blob,
        &canonical_json(&(FORMAT_VERSION, selected))?,
    )
    .digest)
}

fn lookup_entry(
    store: &LocalStore,
    root: &ObjectRef,
    path: &[Vec<u8>],
) -> Result<Option<DirectoryEntry>> {
    let mut directory = root.clone();
    for (index, component) in path.iter().enumerate() {
        let entries = store.read_directory(&directory)?.entries;
        let Some(entry) = entries
            .into_iter()
            .find(|entry| entry.name_hex == hex::encode(component))
        else {
            return Ok(None);
        };
        if index + 1 == path.len() {
            return Ok(Some(entry));
        }
        let EntryNode::Directory { object } = entry.node else {
            bail!("partition traverses a non-directory; symlink expansion is not assumed");
        };
        directory = object;
    }
    Ok(None)
}

impl WorkspaceManager {
    pub fn open(root: impl AsRef<Path>, store: LocalStore) -> Result<Self> {
        let absolute = if root.as_ref().is_absolute() {
            root.as_ref().to_path_buf()
        } else {
            std::env::current_dir()?.join(root.as_ref())
        };
        create_dirs_durable(&absolute)?;
        ensure_real_directory(&absolute)?;
        let marker = absolute.join("manager.json");
        if marker.try_exists()? {
            let value: serde_json::Value = read_checked(&marker)?;
            ensure!(
                value
                    == serde_json::json!({"format_version":1,"role":"private_workspace_manager","store_root":store.root()}),
                "workspace manager marker/store binding mismatch"
            );
        } else {
            ensure!(
                fs::read_dir(&absolute)?.next().is_none(),
                "workspace manager requires a fresh dedicated directory, never a checkout"
            );
            write_checked(
                &marker,
                &serde_json::json!({"format_version":1,"role":"private_workspace_manager","store_root":store.root()}),
            )?;
        }
        #[cfg(unix)]
        fs::set_permissions(&absolute, fs::Permissions::from_mode(0o700))?;
        let root = absolute.canonicalize()?;
        for directory in ["prepared", "views", "layers"] {
            create_dirs_durable(&root.join(directory))?;
            ensure_real_directory(&root.join(directory))?;
        }
        sync_dir(&root)?;
        Ok(Self { root, store })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn store(&self) -> &LocalStore {
        &self.store
    }
    pub fn view_path(&self, id: &str) -> Result<PathBuf> {
        valid_uuid(id)?;
        ensure_real_directory(&self.root.join("views"))?;
        let path = self.root.join("views").join(id);
        if fs::symlink_metadata(&path).is_ok() {
            ensure_real_directory(&path)?;
        }
        Ok(path)
    }
    pub fn execution_path(&self, id: &str) -> Result<PathBuf> {
        let private = self.view_path(id)?.join("private");
        if fs::symlink_metadata(&private).is_ok() {
            ensure_real_directory(&private)?;
        }
        Ok(private.join("merged"))
    }
    fn prepared_path(&self, id: &str) -> Result<PathBuf> {
        valid_digest(id)?;
        ensure_real_directory(&self.root.join("prepared"))?;
        let path = self.root.join("prepared").join(id);
        if fs::symlink_metadata(&path).is_ok() {
            ensure_real_directory(&path)?;
        }
        Ok(path)
    }

    pub fn prepare(&self, request: PrepareRequest) -> Result<PreparedWorkspace> {
        request.profile.validate()?;
        ensure!(
            request.source.kind == ObjectKind::SourceRoot,
            "workspace source must be a SourceRoot"
        );
        let key = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&request)?).digest;
        let _lock = self.lock()?;
        let directory = self.prepared_path(&key)?;
        let receipt = directory.join("prepared.json");
        if receipt.try_exists()? {
            return self.prepared(&key);
        }
        let stage = self.store.begin_staging(Duration::from_secs(3600))?;
        let outcome = (|| {
            let source: SourceManifest =
                serde_json::from_slice(&self.store.read_object(&request.source)?)?;
            let source_directory = source.directory;
            let mut combined = source_directory.clone();
            let mut environment_directory = None;
            if let Some(reference) = &request.environment {
                ensure!(
                    reference.kind == ObjectKind::EnvironmentRoot,
                    "workspace environment must be an EnvironmentRoot"
                );
                let environment: EnvironmentManifest =
                    serde_json::from_slice(&self.store.read_object(reference)?)?;
                ensure!(
                    environment
                        .platform
                        .get("os")
                        .is_some_and(|value| value == std::env::consts::OS)
                        && environment
                            .platform
                            .get("arch")
                            .is_some_and(|value| value == std::env::consts::ARCH),
                    "prepared environment platform/architecture does not match this host"
                );
                self.validate_environment(
                    &environment.directory,
                    &mut Vec::new(),
                    &request.profile,
                )?;
                combined = self.merge_directories(&combined, &environment.directory)?;
                environment_directory = Some(environment.directory);
            }
            let profile = self.store.put_blob(&canonical_json(&request.profile)?)?;
            let mut execution = combined.clone();
            let mut remint = false;
            for path in &request.profile.vcs_local {
                if self.lookup(&execution, path)?.is_some() {
                    execution = self.store.update_path(&execution, path, None)?;
                    remint = true;
                }
            }
            self.validate_links(&execution, &mut Vec::new())?;
            create_dirs_durable(&directory)?;
            ensure_real_directory(&directory)?;
            let lower_layers = if request.backend == WorkspaceBackend::LinuxOverlay {
                let mut source_execution = source_directory;
                for path in &request.profile.vcs_local {
                    if self.lookup(&source_execution, path)?.is_some() {
                        source_execution = self.store.update_path(&source_execution, path, None)?;
                    }
                }
                let mut layers = vec![self.materialize_layer(&source_execution)?];
                if let Some(environment) = &environment_directory {
                    let layer = self.materialize_layer(environment)?;
                    if !layers.contains(&layer) {
                        layers.push(layer);
                    }
                }
                layers
            } else {
                let lower = directory.join("lower");
                if lower.try_exists()? {
                    fs::remove_dir_all(&lower)?;
                }
                self.store.export_tree(&execution, &lower)?;
                vec![lower]
            };
            let lower = lower_layers[0].clone();
            let root_mode = self.store.read_directory(&execution)?.mode;
            let prepared = PreparedWorkspace {
                format_version: FORMAT_VERSION,
                prepared_id: key.clone(),
                source: request.source.clone(),
                environment: request.environment.clone(),
                profile: profile.clone(),
                combined_root: combined.clone(),
                execution_root: execution.clone(),
                backend: request.backend,
                lower,
                lower_layers,
                vcs_remint_required: remint || !request.profile.vcs_local.is_empty(),
                origin_checkpoint: None,
                root_mode,
            };
            // Separate pins intentionally retain partial preparations on a crash.
            for (label, reference) in [
                ("source", &request.source),
                ("profile", &profile),
                ("combined", &combined),
                ("execution", &execution),
            ] {
                self.store
                    .pin(&format!("prepared:{key}:{label}"), reference)?;
            }
            if let Some(environment) = &request.environment {
                self.store
                    .pin(&format!("prepared:{key}:environment"), environment)?;
            }
            write_checked(&receipt, &prepared)?;
            Ok(prepared)
        })();
        self.store.abandon_staging(&stage.id)?;
        outcome
    }

    /// Reconstruct the complete private state, including dirty prepared/scratch
    /// data, from checkpoint-v2. Recovery VCS bytes remain retained in view_root
    /// but are stripped from the runnable tree before the remint bridge runs.
    pub fn prepare_checkpoint(
        &self,
        checkpoint: &ObjectRef,
        backend: WorkspaceBackend,
    ) -> Result<PreparedWorkspace> {
        ensure!(
            checkpoint.kind == ObjectKind::Checkpoint,
            "restore requires a checkpoint"
        );
        let stage = self.store.begin_staging(Duration::from_secs(3600))?;
        let outcome = (|| {
            let value: CheckpointV2Manifest =
                serde_json::from_slice(&self.store.read_object(checkpoint)?)?;
            ensure!(
                value.format_version == 2,
                "full private-view restore requires checkpoint schema 2"
            );
            let profile: WorkspaceProfile =
                serde_json::from_slice(&self.store.read_object(&value.profile)?)?;
            profile.validate()?;
            let key = ObjectRef::from_bytes(
                ObjectKind::Blob,
                &canonical_json(&("restore_checkpoint_v2", checkpoint, backend))?,
            )
            .digest;
            let _lock = self.lock()?;
            let directory = self.prepared_path(&key)?;
            let receipt = directory.join("prepared.json");
            if receipt.try_exists()? {
                return self.prepared(&key);
            }
            if let Some(environment) = &value.environment {
                let environment: EnvironmentManifest =
                    serde_json::from_slice(&self.store.read_object(environment)?)?;
                ensure!(
                    environment
                        .platform
                        .get("os")
                        .is_some_and(|os| os == std::env::consts::OS)
                        && environment
                            .platform
                            .get("arch")
                            .is_some_and(|arch| arch == std::env::consts::ARCH),
                    "checkpoint environment platform/architecture differs from this host"
                );
            }
            let mut execution = value.view_root.clone();
            for path in &profile.vcs_local {
                if self.lookup(&execution, path)?.is_some() {
                    execution = self.store.update_path(&execution, path, None)?;
                }
            }
            self.validate_links(&execution, &mut Vec::new())?;
            create_dirs_durable(&directory)?;
            ensure_real_directory(&directory)?;
            let lower = directory.join("lower");
            if lower.try_exists()? {
                fs::remove_dir_all(&lower)?;
            }
            self.store.export_tree(&execution, &lower)?;
            let root_mode = self.store.read_directory(&execution)?.mode;
            let prepared = PreparedWorkspace {
                format_version: FORMAT_VERSION,
                prepared_id: key.clone(),
                source: value.source.clone(),
                environment: value.environment.clone(),
                profile: value.profile.clone(),
                combined_root: value.view_root.clone(),
                execution_root: execution.clone(),
                backend,
                lower: lower.clone(),
                lower_layers: vec![lower],
                vcs_remint_required: !profile.vcs_local.is_empty(),
                origin_checkpoint: Some(checkpoint.clone()),
                root_mode,
            };
            for (label, reference) in [
                ("source", &value.source),
                ("profile", &value.profile),
                ("combined", &value.view_root),
                ("execution", &execution),
                ("origin", checkpoint),
            ] {
                self.store
                    .pin(&format!("prepared:{key}:{label}"), reference)?;
            }
            if let Some(environment) = &value.environment {
                self.store
                    .pin(&format!("prepared:{key}:environment"), environment)?;
            }
            write_checked(&receipt, &prepared)?;
            Ok(prepared)
        })();
        self.store.abandon_staging(&stage.id)?;
        outcome
    }

    pub fn create(&self, request: CreateRequest) -> Result<WorkspaceDescriptor> {
        valid_uuid(&request.view_id)?;
        valid_uuid(&request.operation_id)?;
        request.owner.validate()?;
        valid_digest(&request.prepared_id)?;
        let _lock = self.lock()?;
        let prepared = self.prepared(&request.prepared_id)?;
        let path = self.view_path(&request.view_id)?;
        let creation_digest =
            ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&request)?).digest;
        let mut descriptor = if path.join("view.json").try_exists()? {
            let existing = self.read_descriptor(&request.view_id)?;
            self.owner(&existing, &request.owner)?;
            ensure!(
                existing.creation_digest == creation_digest,
                "view idempotency key conflicts with another creation"
            );
            if existing.status != WorkspaceStatus::Preparing {
                return Ok(existing);
            }
            existing
        } else {
            create_dirs_durable(&path)?;
            let descriptor = WorkspaceDescriptor {
                format_version: FORMAT_VERSION,
                view_id: request.view_id.clone(),
                owner: request.owner.clone(),
                revision: 0,
                creation_digest,
                prepared_id: request.prepared_id,
                status: WorkspaceStatus::Preparing,
                active_helper: None,
                latest_checkpoint: prepared.origin_checkpoint.clone(),
                operations: BTreeMap::new(),
            };
            self.save(&descriptor)?;
            descriptor
        };
        // No traversal: only existing retained roots may enter the fork fast path.
        for (label, reference) in [
            ("source", &prepared.source),
            ("combined", &prepared.combined_root),
            ("execution", &prepared.execution_root),
            ("profile", &prepared.profile),
        ] {
            self.store
                .pin_existing(&format!("view:{}:{label}", request.view_id), reference)?;
        }
        if let Some(environment) = &prepared.environment {
            self.store.pin_existing(
                &format!("view:{}:environment", request.view_id),
                environment,
            )?;
        }
        if let Some(checkpoint) = &prepared.origin_checkpoint {
            self.store
                .pin_existing(&format!("view:{}:origin", request.view_id), checkpoint)?;
        }
        for name in ["private", "private/upper", "private/work", "private/merged"] {
            create_dirs_durable(&path.join(name))?;
            ensure_real_directory(&path.join(name))?;
        }
        #[cfg(unix)]
        fs::set_permissions(
            path.join("private/upper"),
            fs::Permissions::from_mode(prepared.root_mode),
        )?;
        descriptor.status = WorkspaceStatus::Created;
        descriptor.revision += 1;
        descriptor.operations.insert(
            request.operation_id.clone(),
            WorkspaceReceipt {
                operation_id: request.operation_id,
                operation: "create".into(),
                checkpoint: None,
                revision: descriptor.revision,
                knowledge_cursor: None,
            },
        );
        self.save(&descriptor)?;
        Ok(descriptor)
    }

    pub fn descriptor(&self, id: &str, owner: &OwnerEpoch) -> Result<WorkspaceDescriptor> {
        let _lock = self.lock()?;
        let descriptor = self.read_descriptor(id)?;
        self.owner(&descriptor, owner)?;
        Ok(descriptor)
    }

    /// Trusted authority transition only. The journal must authorize next_owner;
    /// this filesystem layer never allocates epochs or treats inactivity as one.
    pub fn reassign_owner(
        &self,
        id: &str,
        current_owner: &OwnerEpoch,
        next_owner: OwnerEpoch,
        operation_id: &str,
    ) -> Result<WorkspaceReceipt> {
        valid_uuid(operation_id)?;
        current_owner.validate()?;
        next_owner.validate()?;
        ensure!(
            next_owner.epoch > current_owner.epoch,
            "owner epochs must advance"
        );
        let _lock = self.lock()?;
        let _inactive = self.inactive(id)?;
        let mut descriptor = self.read_descriptor(id)?;
        if descriptor.owner == next_owner {
            if let Some(receipt) = descriptor.operations.get(operation_id) {
                ensure!(
                    receipt.operation == "reassign_owner",
                    "operation idempotency conflict"
                );
                return Ok(receipt.clone());
            }
        }
        self.owner(&descriptor, current_owner)?;
        ensure!(descriptor.status != WorkspaceStatus::Running, "an interrupted running helper requires explicit completion proof before ownership can move");
        ensure!(
            !descriptor.operations.contains_key(operation_id),
            "operation idempotency conflict"
        );
        descriptor.owner = next_owner;
        descriptor.revision += 1;
        let receipt = WorkspaceReceipt {
            operation_id: operation_id.into(),
            operation: "reassign_owner".into(),
            checkpoint: descriptor.latest_checkpoint.clone(),
            revision: descriptor.revision,
            knowledge_cursor: None,
        };
        descriptor
            .operations
            .insert(operation_id.into(), receipt.clone());
        self.save(&descriptor)?;
        Ok(receipt)
    }

    pub fn materialize(&self, id: &str, owner: &OwnerEpoch) -> Result<MaterializationReport> {
        let _lock = self.lock()?;
        let _inactive = self.inactive(id)?;
        let mut descriptor = self.read_descriptor(id)?;
        self.owner(&descriptor, owner)?;
        ensure!(
            !matches!(
                descriptor.status,
                WorkspaceStatus::Destroyed | WorkspaceStatus::Sealed | WorkspaceStatus::Running
            ),
            "workspace cannot be materialized in its current state"
        );
        let prepared = self.prepared(&descriptor.prepared_id)?;
        ensure!(
            prepared.backend != WorkspaceBackend::LinuxOverlay,
            "Linux overlay materialization requires its fresh namespace helper"
        );
        let destination = self.execution_path(id)?;
        if descriptor.status == WorkspaceStatus::Created {
            fs::remove_dir(&destination)?;
            let result = match prepared.backend {
                WorkspaceBackend::EagerCopy => self
                    .store
                    .export_tree(&prepared.execution_root, &destination),
                WorkspaceBackend::MacApfsClone => {
                    materialize::apfs_clone_tree(&prepared.lower, &destination)
                }
                WorkspaceBackend::LinuxOverlay => unreachable!(),
            };
            if result.is_err() {
                let _ = fs::remove_dir_all(&destination);
                fs::create_dir(&destination)?;
            }
            result?;
            descriptor.status = WorkspaceStatus::Materialized;
            descriptor.revision += 1;
            self.save(&descriptor)?;
        }
        Ok(MaterializationReport {
            backend: prepared.backend,
            path: destination,
            inherited_bytes_copied: prepared.backend == WorkspaceBackend::EagerCopy,
            entry_count_work: true,
            vcs_remint_required: prepared.vcs_remint_required,
        })
    }

    /// Explicit caller assertion for an otherwise unmanaged/native materialized
    /// view. It is NOT a process barrier. Managed Linux helpers use the stronger
    /// PID-namespace writer-exit path and cannot use this while running.
    pub fn checkpoint_quiesced(
        &self,
        id: &str,
        owner: &OwnerEpoch,
        operation_id: &str,
        seal: bool,
        writers_quiesced: bool,
        knowledge_cursor: u64,
    ) -> Result<WorkspaceReceipt> {
        ensure!(
            writers_quiesced,
            "checkpoint requires all writers, including child processes, to be quiesced"
        );
        let _lock = self.lock()?;
        let _inactive = self.inactive(id)?;
        let descriptor = self.read_descriptor(id)?;
        self.owner(&descriptor, owner)?;
        let prepared = self.prepared(&descriptor.prepared_id)?;
        ensure!(
            prepared.backend != WorkspaceBackend::LinuxOverlay,
            "an overlay checkpoint must be captured inside its namespace helper"
        );
        ensure!(
            descriptor.status != WorkspaceStatus::Running,
            "a crashed/running helper requires recovery, not a caller quiescence assertion"
        );
        self.capture(
            id,
            owner,
            operation_id,
            seal,
            knowledge_cursor,
            &self.execution_path(id)?,
        )
    }

    pub fn destroy(
        &self,
        id: &str,
        owner: &OwnerEpoch,
        operation_id: &str,
    ) -> Result<WorkspaceReceipt> {
        valid_uuid(operation_id)?;
        let _lock = self.lock()?;
        let _inactive = self.inactive(id)?;
        let mut descriptor = self.read_descriptor(id)?;
        self.owner(&descriptor, owner)?;
        if let Some(receipt) = descriptor.operations.get(operation_id) {
            ensure!(
                receipt.operation == "destroy",
                "operation idempotency conflict"
            );
            self.release_base_pins(id)?;
            return Ok(receipt.clone());
        }
        ensure!(
            descriptor.status != WorkspaceStatus::Running,
            "stop and checkpoint the managed helper before destroy"
        );
        let private = self.view_path(id)?.join("private");
        if private.try_exists()? {
            ensure_real_directory(&private)?;
            fs::remove_dir_all(&private)?;
        }
        descriptor.status = WorkspaceStatus::Destroyed;
        descriptor.revision += 1;
        let receipt = WorkspaceReceipt {
            operation_id: operation_id.into(),
            operation: "destroy".into(),
            checkpoint: descriptor.latest_checkpoint.clone(),
            revision: descriptor.revision,
            knowledge_cursor: None,
        };
        descriptor
            .operations
            .insert(operation_id.into(), receipt.clone());
        self.save(&descriptor)?;
        self.release_base_pins(id)?;
        // Checkpoint pins and descriptor receipts intentionally survive destroy.
        Ok(receipt)
    }

    pub fn helper_spec(
        &self,
        id: &str,
        owner: &OwnerEpoch,
        command: Vec<String>,
        vcs_remint_command: Option<Vec<String>>,
        allow_unversioned: bool,
    ) -> Result<WorkspaceHelperSpec> {
        let descriptor = self.descriptor(id, owner)?;
        ensure!(
            !matches!(
                descriptor.status,
                WorkspaceStatus::Destroyed | WorkspaceStatus::Sealed
            ),
            "workspace is not executable"
        );
        ensure!(
            !command.is_empty() && command.iter().all(|part| !part.contains('\0')),
            "invalid helper command"
        );
        if let Some(bridge) = &vcs_remint_command {
            ensure!(
                !bridge.is_empty() && bridge.iter().all(|part| !part.contains('\0')),
                "invalid VCS bridge command"
            );
        }
        Ok(WorkspaceHelperSpec {
            manager_root: self.root.clone(),
            store_root: self.store.root().to_path_buf(),
            view_id: id.into(),
            owner: owner.clone(),
            command,
            vcs_remint_command,
            allow_unversioned,
        })
    }

    fn capture(
        &self,
        id: &str,
        owner: &OwnerEpoch,
        operation_id: &str,
        seal: bool,
        knowledge_cursor: u64,
        visible: &Path,
    ) -> Result<WorkspaceReceipt> {
        valid_uuid(operation_id)?;
        let mut descriptor = self.read_descriptor(id)?;
        self.owner(&descriptor, owner)?;
        let operation = if seal { "seal" } else { "checkpoint" };
        if let Some(receipt) = descriptor.operations.get(operation_id) {
            ensure!(
                receipt.operation == operation
                    && receipt.knowledge_cursor == Some(knowledge_cursor),
                "operation idempotency conflict"
            );
            return Ok(receipt.clone());
        }
        ensure!(
            !matches!(
                descriptor.status,
                WorkspaceStatus::Preparing
                    | WorkspaceStatus::Created
                    | WorkspaceStatus::Destroyed
                    | WorkspaceStatus::Sealed
            ),
            "workspace cannot be checkpointed in this state"
        );
        let prepared = self.prepared(&descriptor.prepared_id)?;
        let profile: WorkspaceProfile =
            serde_json::from_slice(&self.store.read_object(&prepared.profile)?)?;
        profile.validate()?;
        let stage = self.store.begin_staging(Duration::from_secs(3600))?;
        let result = (|| {
            let view_root = self
                .store
                .import_tree(visible, CheckpointConsistency::Quiesced)?;
            let source_tree = self.store.import_tree_with_policy(
                visible,
                CheckpointConsistency::Quiesced,
                &profile.source_exclusions(),
            )?;
            let source_parent = if let Some(checkpoint) = &descriptor.latest_checkpoint {
                let previous: serde_json::Value =
                    serde_json::from_slice(&self.store.read_object(checkpoint)?)?;
                serde_json::from_value(
                    previous
                        .get("source")
                        .context("checkpoint has no source")?
                        .clone(),
                )?
            } else {
                prepared.source.clone()
            };
            let source = self.store.put_source(&SourceManifest {
                format_version: FORMAT_VERSION,
                directory: source_tree,
                parents: vec![source_parent],
                consistency: CheckpointConsistency::Quiesced,
            })?;
            let checkpoint = self.store.put_checkpoint_v2(&CheckpointV2Manifest {
                format_version: 2,
                source,
                environment: prepared.environment.clone(),
                parents: descriptor.latest_checkpoint.iter().cloned().collect(),
                consistency: CheckpointConsistency::Quiesced,
                durability: Durability::Local,
                knowledge_cursor,
                view_root,
                profile: prepared.profile.clone(),
            })?;
            self.store.publish(
                &stage.id,
                &format!("view:{id}:checkpoint:{operation_id}"),
                &checkpoint,
            )?;
            descriptor.latest_checkpoint = Some(checkpoint.clone());
            descriptor.status = if seal {
                WorkspaceStatus::Sealed
            } else {
                WorkspaceStatus::Quiesced
            };
            descriptor.active_helper = None;
            descriptor.revision += 1;
            let receipt = WorkspaceReceipt {
                operation_id: operation_id.into(),
                operation: operation.into(),
                checkpoint: Some(checkpoint),
                revision: descriptor.revision,
                knowledge_cursor: Some(knowledge_cursor),
            };
            descriptor
                .operations
                .insert(operation_id.into(), receipt.clone());
            self.save(&descriptor)?;
            Ok(receipt)
        })();
        if result.is_err() {
            let _ = self.store.abandon_staging(&stage.id);
        }
        result
    }

    fn prepared(&self, id: &str) -> Result<PreparedWorkspace> {
        valid_digest(id)?;
        let prepared: PreparedWorkspace =
            read_checked(&self.prepared_path(id)?.join("prepared.json"))?;
        ensure!(
            prepared.format_version == FORMAT_VERSION && prepared.prepared_id == id,
            "invalid prepared receipt"
        );
        ensure!(
            !prepared.lower_layers.is_empty()
                && prepared.lower_layers.len() <= 2
                && prepared.lower_layers[0] == prepared.lower,
            "invalid prepared lower-layer descriptor"
        );
        for lower in &prepared.lower_layers {
            if lower != &self.root.join("prepared").join(id).join("lower") {
                let relative = lower
                    .strip_prefix(self.root.join("layers"))
                    .context("prepared lower path escaped ownership")?;
                let components: Vec<_> = relative.components().collect();
                ensure!(
                    components.len() == 2 && components[1].as_os_str() == "tree",
                    "invalid retained layer path"
                );
                valid_digest(
                    components[0]
                        .as_os_str()
                        .to_str()
                        .context("invalid layer identity")?,
                )?;
                ensure_real_directory(&self.root.join("layers"))?;
                ensure_real_directory(lower.parent().unwrap())?;
            }
            ensure_real_directory(lower)?;
        }
        Ok(prepared)
    }

    fn materialize_layer(&self, directory: &ObjectRef) -> Result<PathBuf> {
        ensure!(
            directory.kind == ObjectKind::Directory,
            "lower layer must be an immutable directory"
        );
        ensure_real_directory(&self.root.join("layers"))?;
        let cache = self.root.join("layers").join(&directory.digest);
        create_dirs_durable(&cache)?;
        ensure_real_directory(&cache)?;
        let receipt = cache.join("layer.json");
        let tree = cache.join("tree");
        if receipt.try_exists()? {
            let saved: ObjectRef = read_checked(&receipt)?;
            ensure!(saved == *directory, "retained layer identity mismatch");
            ensure_real_directory(&tree)?;
            return Ok(tree);
        }
        if tree.try_exists()? {
            ensure_real_directory(&tree)?;
            fs::remove_dir_all(&tree)?;
        }
        self.store.export_tree(directory, &tree)?;
        self.store
            .pin(&format!("workspace-layer:{}", directory.digest), directory)?;
        write_checked(&receipt, directory)?;
        Ok(tree)
    }
    fn read_descriptor(&self, id: &str) -> Result<WorkspaceDescriptor> {
        let descriptor: WorkspaceDescriptor = read_checked(&self.view_path(id)?.join("view.json"))?;
        ensure!(
            descriptor.format_version == FORMAT_VERSION && descriptor.view_id == id,
            "invalid workspace descriptor"
        );
        descriptor.owner.validate()?;
        Ok(descriptor)
    }
    fn save(&self, descriptor: &WorkspaceDescriptor) -> Result<()> {
        write_checked(
            &self.view_path(&descriptor.view_id)?.join("view.json"),
            descriptor,
        )
    }
    fn owner(&self, descriptor: &WorkspaceDescriptor, owner: &OwnerEpoch) -> Result<()> {
        owner.validate()?;
        ensure!(descriptor.owner == *owner, "workspace owner epoch is stale");
        Ok(())
    }
    fn release_base_pins(&self, id: &str) -> Result<()> {
        for label in [
            "source",
            "environment",
            "combined",
            "execution",
            "profile",
            "origin",
        ] {
            self.store.unpin(&format!("view:{id}:{label}"))?;
        }
        Ok(())
    }
    fn lock(&self) -> Result<File> {
        ensure_real_directory(&self.root)?;
        let file = regular_open(&self.root.join("manager.lock"), true)?;
        file.lock_exclusive()?;
        Ok(file)
    }
    fn inactive(&self, id: &str) -> Result<File> {
        let file = regular_open(&self.view_path(id)?.join("run.lock"), true)?;
        file.try_lock_exclusive()
            .context("workspace helper still owns writers")?;
        Ok(file)
    }

    fn lookup(&self, root: &ObjectRef, path: &[Vec<u8>]) -> Result<Option<DirectoryEntry>> {
        lookup_entry(&self.store, root, path)
    }
    fn merge_directories(&self, source: &ObjectRef, environment: &ObjectRef) -> Result<ObjectRef> {
        let mut left = self.store.read_directory(source)?;
        let right = self.store.read_directory(environment)?;
        let mut entries: BTreeMap<_, _> = left
            .entries
            .into_iter()
            .map(|entry| (entry.name_hex.clone(), entry))
            .collect();
        for entry in right.entries {
            if let Some(existing) = entries.get_mut(&entry.name_hex) {
                match (&existing.node, &entry.node) {
                    (EntryNode::Directory { object: a }, EntryNode::Directory { object: b }) => {
                        existing.node = EntryNode::Directory {
                            object: self.merge_directories(a, b)?,
                        };
                    }
                    _ if *existing == entry => {}
                    _ => bail!("prepared environment conflicts with source"),
                }
            } else {
                entries.insert(entry.name_hex.clone(), entry);
            }
        }
        left.entries = entries.into_values().collect();
        self.store.put_directory(left)
    }
    fn validate_environment(
        &self,
        root: &ObjectRef,
        path: &mut RawPath,
        profile: &WorkspaceProfile,
    ) -> Result<()> {
        for entry in self.store.read_directory(root)?.entries {
            path.push(entry.name()?);
            let allowed = profile
                .prepared
                .iter()
                .any(|prefix| path.starts_with(prefix));
            match entry.node {
                EntryNode::Directory { object } => {
                    ensure!(
                        allowed
                            || profile
                                .prepared
                                .iter()
                                .any(|prefix| prefix.starts_with(path)),
                        "environment directory lies outside prepared partitions"
                    );
                    self.validate_environment(&object, path, profile)?;
                }
                _ => ensure!(allowed, "environment file lies outside prepared partitions"),
            }
            path.pop();
        }
        Ok(())
    }
    fn validate_links(&self, root: &ObjectRef, path: &mut RawPath) -> Result<()> {
        for entry in self.store.read_directory(root)?.entries {
            let name = entry.name()?;
            match entry.node {
                EntryNode::Directory { object } => {
                    path.push(name);
                    self.validate_links(&object, path)?;
                    path.pop();
                }
                EntryNode::Symlink { target_hex } => {
                    let target = hex::decode(target_hex)?;
                    ensure!(
                        !target.starts_with(b"/"),
                        "absolute symlink requires an explicitly scoped platform bridge"
                    );
                    let mut depth = path.len();
                    for component in target.split(|byte| *byte == b'/') {
                        if component == b".." {
                            ensure!(depth > 0, "symlink leaves managed workspace");
                            depth -= 1;
                        } else if !component.is_empty() && component != b"." {
                            depth += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn validate_name(name: &[u8]) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name != b"."
            && name != b".."
            && !name.contains(&0)
            && !name.contains(&b'/'),
        "unsafe partition component"
    );
    Ok(())
}
fn valid_uuid(value: &str) -> Result<()> {
    ensure!(
        Uuid::parse_str(value).is_ok() && value.len() == 36 && value == value.to_lowercase(),
        "workspace/operation id must be a lowercase UUID"
    );
    Ok(())
}
fn valid_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid prepared identity"
    );
    Ok(())
}
fn ensure_real_directory(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.is_dir(),
        "managed path must be a real directory"
    );
    Ok(())
}
fn regular_open(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(create).create(create);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "expected a regular managed file"
    );
    Ok(file)
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
fn create_dirs_durable(path: &Path) -> Result<()> {
    if path.try_exists()? {
        ensure!(path.is_dir(), "managed ancestor is not a directory");
        return Ok(());
    }
    let parent = path.parent().context("managed directory has no parent")?;
    create_dirs_durable(parent)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure_real_directory(path)?
        }
        Err(error) => return Err(error.into()),
    }
    sync_dir(path)?;
    sync_dir(parent)?;
    Ok(())
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checked<T> {
    value: T,
    checksum: String,
}
fn read_checked<T: for<'de> Deserialize<'de> + Serialize>(path: &Path) -> Result<T> {
    let mut file = regular_open(path, false)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let value: Checked<T> = serde_json::from_slice(&bytes)?;
    ensure!(
        value.checksum
            == ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&value.value)?).digest,
        "managed descriptor checksum mismatch"
    );
    Ok(value.value)
}
fn write_checked<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let checksum = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(value)?).digest;
    let bytes = canonical_json(&Checked { value, checksum })?;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_dir(path.parent().unwrap())?;
    Ok(())
}
