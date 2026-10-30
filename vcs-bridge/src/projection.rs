//! Explicit, metered immutable Directory ↔ pinned jj tree conversion.
//!
//! This is a full metadata traversal, not a source walk or an O(changes)
//! namespace promise. An owned prior receipt can reuse unchanged file IDs and
//! exact conflict-marker lengths. Matchers and ignore objects are evaluated on
//! every snapshot; their Debug output or address is never a policy identity.

use crate::JjTreeIdentity;
use crate::projection_io::{self, MeteredSource, ProjectionBudget, ProjectionWork};
use anyhow::{Context, Result, bail, ensure};
use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::conflicts::{self, ConflictMarkerStyle, ConflictMaterializeOptions};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::merge::{Merge, MergedTreeValue, SameChange};
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::working_copy::{SnapshotOptions, SnapshotStats, UntrackedReason};
use rudder_engine::content::{self, StagedContentStore};
use rudder_engine::directory::{
    self, DirectoryEntry, EntryNode, LocalDirectoryStore, PagedDirectory,
};
use rudder_engine::protocol::{ObjectKind, ObjectRef, canonical_json};
use rudder_engine::store::{DirectoryManifest, LocalStore};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedPath {
    pub source: EntryNode,
    /// The actual length used when these source bytes were materialized. Keep
    /// this after a partial resolution even if the new sides need shorter
    /// markers; recomputing it can turn a remaining conflict into plain text.
    pub conflict_marker_len: Option<usize>,
}

/// Trusted host-side provenance, not a guest-supplied optimization hint. The
/// owner retains its Directory and writes this beside private attempt state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionRecord {
    pub format_version: u32,
    pub directory: ObjectRef,
    pub tree: JjTreeIdentity,
    pub projection_policy: ObjectRef,
    pub paths: BTreeMap<String, ProjectedPath>,
}
impl ProjectionRecord {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "unsupported source projection record"
        );
        ensure!(
            self.directory.kind == ObjectKind::Directory,
            "projection requires a Directory"
        );
        self.directory.validate()?;
        ensure!(
            self.projection_policy.kind == ObjectKind::Blob,
            "projection policy must be a Blob"
        );
        self.projection_policy.validate()?;
        self.tree.validate()?;
        ensure!(
            self.paths.len() <= 100_000,
            "projection record exceeds supported path bound"
        );
        for (path, entry) in &self.paths {
            let path = checked_path(path)?;
            let name = path.split().context("projection leaf cannot be root")?.1;
            DirectoryEntry::new(name.as_internal_str().as_bytes(), entry.source.clone())?;
            ensure!(
                !matches!(entry.source, EntryNode::Directory { .. }),
                "projection path must be a leaf"
            );
            if let Some(length) = entry.conflict_marker_len {
                ensure!(
                    (conflicts::MIN_CONFLICT_MARKER_LEN..=4096).contains(&length),
                    "unsupported conflict marker length"
                );
                ensure!(
                    matches!(entry.source, EntryNode::File { .. }),
                    "only files can carry conflict markers"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ProjectedSource {
    pub tree: MergedTree,
    pub record: ProjectionRecord,
    pub stats: SnapshotStats,
    pub work: ProjectionWork,
}

/// `None` selects every leaf of an already versioned, sealed source Directory.
/// Passing SnapshotOptions applies current ignore/auto-track/force/size policy
/// to live visible source, while preserving tracked paths and copy identities.
/// This creates only immutable private Git objects; publication is separate.
pub async fn project_source(
    store: LocalStore,
    directory: &ObjectRef,
    baseline: &MergedTree,
    projection_policy: &ObjectRef,
    options: Option<&SnapshotOptions<'_>>,
    previous: Option<&ProjectionRecord>,
    budget: ProjectionBudget,
) -> Result<ProjectedSource> {
    project_source_inner(
        store,
        directory,
        baseline,
        projection_policy,
        options,
        previous,
        budget,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn project_source_inner(
    store: LocalStore,
    directory: &ObjectRef,
    baseline: &MergedTree,
    projection_policy: &ObjectRef,
    options: Option<&SnapshotOptions<'_>>,
    previous: Option<&ProjectionRecord>,
    budget: ProjectionBudget,
    allow_file_reuse: bool,
) -> Result<ProjectedSource> {
    ensure!(
        directory.kind == ObjectKind::Directory,
        "source must be a Directory"
    );
    directory.validate()?;
    projection_policy.validate()?;
    ensure!(
        projection_policy.kind == ObjectKind::Blob,
        "projection policy must be a Blob"
    );
    let source = MeteredSource::new(store, budget)?;
    // Discover unsupported baseline values before changing a working-copy
    // receipt or any source namespace. Failed object writes remain unreachable.
    let tracked = read_jj_entries(&source, baseline)?;
    let previous = previous.filter(|record| {
        record.tree == JjTreeIdentity::from_jj(baseline)
            && &record.projection_policy == projection_policy
    });
    if let Some(previous) = previous {
        previous.validate()?;
    }
    let mut selected = BTreeMap::new();
    let mut stats = SnapshotStats::default();
    collect_source(
        &source,
        directory,
        RepoPath::root(),
        0,
        options
            .map(|o| o.base_ignores.clone())
            .unwrap_or_else(GitIgnoreFile::empty),
        false,
        options,
        &tracked,
        &mut selected,
        &mut stats,
    )?;
    let mut builder = MergedTreeBuilder::new(baseline.clone());
    for path in tracked.keys() {
        if !selected.contains_key(path) {
            builder.set_or_remove(path.clone(), Merge::absent());
        }
    }
    let mut paths = BTreeMap::new();
    for (path, node) in selected {
        if let Some(progress) = options.and_then(|o| o.progress) {
            progress(&path);
        }
        let absent = Merge::absent();
        let old = tracked.get(&path).unwrap_or(&absent);
        let prior = previous.and_then(|p| p.paths.get(path.as_internal_file_string()));
        let unchanged =
            allow_file_reuse && prior.is_some_and(|p| p.source == node) && old.is_present();
        let (value, marker) = if unchanged {
            source.reused();
            (old.clone(), prior.and_then(|p| p.conflict_marker_len))
        } else {
            project_leaf(&source, baseline, &path, &node, old, prior).await?
        };
        if &value != old {
            builder.set_or_remove(path.clone(), value);
        }
        paths.insert(
            path.as_internal_file_string().into(),
            ProjectedPath {
                source: node,
                conflict_marker_len: marker,
            },
        );
    }
    let tree = builder.write_tree().await?;
    let record = ProjectionRecord {
        format_version: 1,
        directory: directory.clone(),
        tree: JjTreeIdentity::from_jj(&tree),
        projection_policy: projection_policy.clone(),
        paths,
    };
    record.validate()?;
    Ok(ProjectedSource {
        tree,
        record,
        stats,
        work: source.work(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMappingReceipt {
    pub format_version: u32,
    pub method: String,
    pub source: ObjectRef,
    pub directory: ObjectRef,
    pub projection_policy: ObjectRef,
    pub tree: JjTreeIdentity,
    pub marker_receipt: Option<ObjectRef>,
    pub work: ProjectionWork,
}

/// Constructed only by recomputing actual immutable source→Git mapping. This
/// proves logical bytes, executable bits, symlinks and jj conflict terms. It
/// does not prove the manifest's claimed writer barrier or quarantine an
/// untrusted Git object database. It cannot be deserialized from guest JSON.
#[derive(Debug)]
pub struct VerifiedSourceProjection {
    receipt: SourceMappingReceipt,
    evidence: ObjectRef,
    retention_pin: String,
    store: LocalStore,
}
impl VerifiedSourceProjection {
    pub fn receipt(&self) -> &SourceMappingReceipt {
        &self.receipt
    }
    pub fn evidence(&self) -> &ObjectRef {
        &self.evidence
    }
    pub fn retention_pin(&self) -> &str {
        &self.retention_pin
    }
    /// After the journal has retained this SAME evidence root, release only the
    /// temporary verifier-owned pin. Drop deliberately does not unpin; unknown
    /// cross-store outcomes retain evidence for reconciliation.
    pub fn release_after_journal_retention(&self) -> Result<()> {
        let retention = self.store.retention()?;
        ensure!(
            retention
                .pins
                .iter()
                .any(|(id, root)| id != &self.retention_pin && root == &self.evidence),
            "mapping evidence has not acquired another durable retention owner"
        );
        self.store.unpin(&self.retention_pin)
    }
    pub fn check_proof(&self, proof: &crate::SealedSourceProof) -> Result<()> {
        proof.validate()?;
        ensure!(
            proof.source == self.receipt.source
                && proof.tree == self.receipt.tree
                && proof.binding.projection_policy == self.receipt.projection_policy,
            "source mapping witness differs from the sealed proof"
        );
        validate_mapping_evidence(&self.store, &self.evidence, proof)?;
        Ok(())
    }
}

/// Validate the exact retained verifier receipt at prepare/publish time. This
/// reads immutable facts; it never deserializes an opaque verification object
/// or claims that arbitrary JSON has independently rerun the mapping.
pub(crate) fn validate_mapping_evidence(
    store: &LocalStore,
    reference: &ObjectRef,
    proof: &crate::SealedSourceProof,
) -> Result<()> {
    reference.validate()?;
    ensure!(
        reference.kind == ObjectKind::ActionResult && reference.size_bytes <= 1024 * 1024,
        "projection evidence requires a bounded ActionResult"
    );
    ensure!(
        store
            .retention()?
            .pins
            .values()
            .any(|root| root == reference),
        "mapping evidence is not retained"
    );
    let bytes = store.read_object(reference)?;
    let value: rudder_engine::store::ReferencedMetadata = serde_json::from_slice(&bytes)?;
    ensure!(
        value.format_version == 1
            && canonical_json(&value)? == bytes
            && value.payload["kind"] == "source-jj-projection-v1",
        "invalid projection evidence envelope"
    );
    let receipt: SourceMappingReceipt = serde_json::from_value(value.payload["receipt"].clone())?;
    receipt.tree.validate()?;
    ensure!(
        receipt.source.kind == ObjectKind::SourceRoot
            && receipt.source.size_bytes <= 1024 * 1024
            && receipt.directory.kind == ObjectKind::Directory
            && receipt.projection_policy.kind == ObjectKind::Blob,
        "invalid projection receipt reference kinds/bounds"
    );
    ensure!(
        receipt.format_version == 1
            && receipt.method == "full-verified-stream-v1"
            && receipt.source == proof.source
            && receipt.projection_policy == proof.binding.projection_policy
            && receipt.tree == proof.tree,
        "retained source projection receipt differs from proof"
    );
    let source_bytes = store.read_object(&receipt.source)?;
    let source: serde_json::Value = serde_json::from_slice(&source_bytes)?;
    ensure!(
        source["directory"] == serde_json::to_value(&receipt.directory)?,
        "mapping receipt directory differs from actual source root"
    );
    if source["format_version"] == 2 {
        ensure!(
            source["projection_policy"] == serde_json::to_value(&receipt.projection_policy)?,
            "stored source policy differs"
        );
    }
    ensure!(
        value.references.contains(&receipt.source)
            && value.references.contains(&receipt.projection_policy),
        "projection evidence omits retained source/policy edges"
    );
    if let Some(marker) = &receipt.marker_receipt {
        marker.validate()?;
        ensure!(
            marker.kind == ObjectKind::Blob
                && marker.size_bytes <= 64 * 1024 * 1024
                && value.references.contains(marker),
            "projection evidence omits bounded marker provenance"
        );
        let bytes = store.read_object(marker)?;
        let marker: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(
            canonical_json(&marker)? == bytes,
            "marker provenance must be canonical"
        );
        ensure!(
            marker["format_version"] == 1
                && marker["kind"] == "source-jj-conflict-markers-v1"
                && marker["projection_policy"] == serde_json::to_value(&receipt.projection_policy)?
                && marker["tree"] == serde_json::to_value(&receipt.tree)?,
            "marker provenance differs from projection"
        );
        let directory: ObjectRef = serde_json::from_value(marker["source_directory"].clone())?;
        directory.validate()?;
        ensure!(
            directory.kind == ObjectKind::Directory
                && directory == receipt.directory
                && value.references.contains(&directory),
            "marker source provenance is not retained"
        );
        let lengths = marker["lengths"]
            .as_object()
            .context("marker lengths must be a path map")?;
        ensure!(lengths.len() <= 100_000, "too many conflict marker paths");
        for (path, length) in lengths {
            checked_path(path)?;
            ensure!(
                length
                    .as_u64()
                    .is_some_and(
                        |value| (conflicts::MIN_CONFLICT_MARKER_LEN as u64..=4096).contains(&value)
                    ),
                "invalid retained conflict marker length"
            );
        }
    }
    Ok(())
}

/// Recompute every file mapping; even an owned prior receipt is used ONLY for
/// marker length, never as a cache-based equality assertion. Source v1 retains
/// its old ancestry identity. Schema-2 additionally pins the policy itself.
pub async fn verify_versioned_source(
    store: LocalStore,
    staging_id: &str,
    source_root: &ObjectRef,
    projection_policy: &ObjectRef,
    target: &MergedTree,
    marker_receipt: Option<&ProjectionRecord>,
    mut budget: ProjectionBudget,
) -> Result<VerifiedSourceProjection> {
    let began = Instant::now();
    source_root.validate()?;
    ensure!(
        source_root.kind == ObjectKind::SourceRoot && source_root.size_bytes <= 1024 * 1024,
        "source mapping requires a bounded SourceRoot"
    );
    ensure!(
        budget.max_elapsed_ms > 0 && source_root.size_bytes <= budget.max_metadata_bytes,
        "source manifest exceeds projection admission budget"
    );
    budget.max_metadata_bytes -= source_root.size_bytes;
    if let Some(marker) = marker_receipt {
        marker.validate()?;
        ensure!(
            marker.tree == JjTreeIdentity::from_jj(target)
                && marker.projection_policy == *projection_policy,
            "marker receipt belongs to another tree/policy"
        );
    }
    let bytes = store.read_object(source_root)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let directory = match value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(2) => {
            let source = rudder_engine::snapshot::decode_source(&bytes)?;
            ensure!(
                source.projection_policy == *projection_policy,
                "stored SourceV2 policy differs from mapping policy"
            );
            source.directory
        }
        Some(1) => {
            let source: rudder_engine::store::SourceManifest = serde_json::from_value(value)?;
            ensure!(source.format_version == 1, "unsupported source manifest");
            source.directory
        }
        _ => bail!("unsupported source manifest version"),
    };
    StagedContentStore::new(&store, staging_id)?;
    let elapsed = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
    ensure!(
        elapsed < budget.max_elapsed_ms,
        "source verification cooperative deadline exceeded"
    );
    budget.max_elapsed_ms -= elapsed;
    let mut projected = project_source_inner(
        store.clone(),
        &directory,
        target,
        projection_policy,
        None,
        marker_receipt,
        budget,
        false,
    )
    .await?;
    projected.work.cas_metadata_reads += 1;
    projected.work.cas_metadata_bytes = projected
        .work
        .cas_metadata_bytes
        .checked_add(source_root.size_bytes)
        .context("source metadata count overflow")?;
    let identity = JjTreeIdentity::from_jj(target);
    ensure!(
        JjTreeIdentity::from_jj(&projected.tree) == identity,
        "actual versioned source does not equal the proposed jj tree"
    );
    let mut references = vec![source_root.clone(), projection_policy.clone()];
    let marker = if let Some(marker) = marker_receipt {
        let lengths: BTreeMap<_, _> = marker
            .paths
            .iter()
            .filter_map(|(path, value)| {
                value
                    .conflict_marker_len
                    .map(|length| (path.clone(), length))
            })
            .collect();
        if lengths.is_empty() {
            None
        } else {
            let bytes = canonical_json(&serde_json::json!({
                "format_version": 1,
                "kind": "source-jj-conflict-markers-v1",
                "source_directory": directory,
                "projection_policy": marker.projection_policy,
                "tree": marker.tree,
                "lengths": lengths,
            }))?;
            ensure!(
                bytes.len() <= 64 * 1024 * 1024,
                "marker provenance exceeds supported bound"
            );
            let reference = ObjectRef::from_bytes(ObjectKind::Blob, &bytes);
            store.receive_staged(staging_id, &reference, &bytes)?;
            references.push(reference.clone());
            references.push(directory.clone());
            Some(reference)
        }
    } else {
        None
    };
    let receipt = SourceMappingReceipt {
        format_version: 1,
        method: "full-verified-stream-v1".into(),
        source: source_root.clone(),
        directory,
        projection_policy: projection_policy.clone(),
        tree: identity,
        marker_receipt: marker,
        work: projected.work,
    };
    references.sort();
    references.dedup();
    let bytes = canonical_json(&rudder_engine::store::ReferencedMetadata {
        format_version: 1,
        references,
        payload: serde_json::json!({ "kind": "source-jj-projection-v1", "receipt": receipt }),
    })?;
    let evidence = ObjectRef::from_bytes(ObjectKind::ActionResult, &bytes);
    store.receive_staged(staging_id, &evidence, &bytes)?;
    let retention_pin = format!("vcs-projection-{}", uuid::Uuid::new_v4());
    store.pin(&retention_pin, &evidence)?;
    Ok(VerifiedSourceProjection {
        receipt,
        evidence,
        retention_pin,
        store,
    })
}

fn checked_path(path: &str) -> Result<RepoPathBuf> {
    let path = RepoPathBuf::from_internal_string(path)?;
    ensure!(
        path.components().count() <= 256,
        "source path exceeds supported depth"
    );
    for component in path.components() {
        ensure!(
            component.as_internal_str().len() <= 255,
            "source path component exceeds supported length"
        );
        ensure!(
            !matches!(
                component.as_internal_str().to_ascii_lowercase().as_str(),
                ".git" | ".jj"
            ),
            "reserved VCS metadata name in source projection"
        );
    }
    Ok(path)
}

fn read_jj_entries(
    source: &MeteredSource,
    tree: &MergedTree,
) -> Result<BTreeMap<RepoPathBuf, MergedTreeValue>> {
    let mut entries = BTreeMap::new();
    for (path, value) in tree.entries() {
        source.depth(path.components().count())?;
        checked_path(path.as_internal_file_string())?;
        let value = value?;
        ensure!(
            value.iter().count() <= 1025,
            "jj value exceeds supported conflict arity"
        );
        source.entry(
            false,
            path.as_internal_file_string().len() + value.iter().count() * 128,
        )?;
        projection_io::reject_submodule(&value)?;
        ensure!(
            value.as_resolved().is_some() || value.to_file_merge().is_some(),
            "non-file conflicts require a separate qualified projection profile"
        );
        ensure!(
            !value
                .iter()
                .flatten()
                .any(|v| matches!(v, TreeValue::Tree(_))),
            "tree/file conflicts are unsupported by this projection profile"
        );
        entries.insert(path, value);
    }
    Ok(entries)
}

fn directory_entries(source: &MeteredSource, root: &ObjectRef) -> Result<Vec<DirectoryEntry>> {
    // Wide legacy arrays need an explicit paged conversion, rather than a
    // monolithic JSON allocation before the entry meter can inspect them.
    ensure!(
        root.size_bytes <= 4 * 1024 * 1024,
        "source directory exceeds bounded legacy metadata profile; use DirectoryV2"
    );
    let bytes = source.read_metadata(root)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    match value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(1) => {
            let manifest: DirectoryManifest = serde_json::from_value(value)?;
            ensure!(
                manifest.format_version == 1 && manifest.mode <= 0o7777,
                "invalid legacy source directory"
            );
            let mut previous = None;
            let mut entries = Vec::new();
            for entry in manifest.entries {
                let node = match entry.node {
                    rudder_engine::store::EntryNode::File { object, mode } => EntryNode::File {
                        logical_size: object.size_bytes,
                        object,
                        mode,
                    },
                    rudder_engine::store::EntryNode::Directory { object } => {
                        EntryNode::Directory { object }
                    }
                    rudder_engine::store::EntryNode::Symlink { target_hex } => {
                        EntryNode::Symlink { target_hex }
                    }
                };
                let entry = DirectoryEntry {
                    name_hex: entry.name_hex,
                    node,
                };
                entry.validate()?;
                ensure!(
                    previous
                        .as_ref()
                        .is_none_or(|p: &String| p < &entry.name_hex),
                    "source directory is not sorted/unique"
                );
                previous = Some(entry.name_hex.clone());
                entries.push(entry);
            }
            Ok(entries)
        }
        Some(2) => {
            directory::decode_directory_root(&bytes)?;
            let directory = PagedDirectory::new(source);
            let mut entries = Vec::new();
            let mut after = None;
            loop {
                let page = directory.scan(root, after.as_deref(), 1024)?;
                entries.extend(page.entries);
                after = page.next_after;
                if after.is_none() {
                    return Ok(entries);
                }
                // The next page must not allocate beyond the requested entry
                // budget before the traversal gets a chance to charge it.
                source.depth(0)?;
                ensure!(
                    entries.len() <= 100_000,
                    "directory exceeds projection path bound"
                );
            }
        }
        _ => bail!("unsupported source directory format"),
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_source(
    source: &MeteredSource,
    root: &ObjectRef,
    prefix: &RepoPath,
    depth: usize,
    mut ignores: Arc<GitIgnoreFile>,
    tracked_only: bool,
    options: Option<&SnapshotOptions<'_>>,
    tracked: &BTreeMap<RepoPathBuf, MergedTreeValue>,
    selected: &mut BTreeMap<RepoPathBuf, EntryNode>,
    stats: &mut SnapshotStats,
) -> Result<()> {
    source.depth(depth)?;
    let entries = directory_entries(source, root)?;
    for entry in &entries {
        source.depth(depth + 1)?;
        source.entry(
            true,
            canonical_json(entry)?.len() + prefix.as_internal_file_string().len() + 128,
        )?;
        let name = entry.name()?;
        let name = std::str::from_utf8(&name).context(
            "jj paths require UTF-8; raw non-UTF-8 names need a separate projection policy",
        )?;
        checked_path(&format!("{}{name}", prefix.to_internal_dir_string()))?;
    }
    if options.is_some() && !tracked_only {
        if let Some(entry) = entries
            .iter()
            .find(|e| e.name_hex == hex::encode(b".gitignore"))
        {
            ensure!(
                !matches!(entry.node, EntryNode::Symlink { .. }),
                "symlink .gitignore needs a qualified immutable symlink resolver"
            );
            if let EntryNode::File {
                object,
                logical_size,
                ..
            } = &entry.node
            {
                ensure!(
                    *logical_size <= 1024 * 1024,
                    ".gitignore exceeds bounded immutable pattern profile"
                );
                let bytes = source.read_all_file(object, *logical_size)?;
                let path = format!("{}.gitignore", prefix.to_internal_dir_string());
                ignores =
                    ignores.chain(&prefix.to_internal_dir_string(), Path::new(&path), &bytes)?;
            }
        }
    }
    for entry in entries {
        let name = String::from_utf8(entry.name()?)?;
        let path = checked_path(&format!("{}{name}", prefix.to_internal_dir_string()))?;
        if let EntryNode::Directory { object } = &entry.node {
            let ignored = options.is_some_and(|o| {
                tracked_only
                    || (ignores.matches(&path.to_internal_dir_string())
                        && o.force_tracking_matcher.visit(&path).is_nothing())
            });
            collect_source(
                source,
                object,
                &path,
                depth + 1,
                ignores.clone(),
                ignored,
                options,
                tracked,
                selected,
                stats,
            )?;
            continue;
        }
        if !tracked.contains_key(&path) {
            if let Some(options) = options {
                if tracked_only
                    || (ignores.matches(path.as_internal_file_string())
                        && !options.force_tracking_matcher.matches(&path))
                {
                    continue;
                }
                if !options.start_tracking_matcher.matches(&path) {
                    stats
                        .untracked_paths
                        .insert(path, UntrackedReason::FileNotAutoTracked);
                    continue;
                }
                let size = match &entry.node {
                    EntryNode::File { logical_size, .. } => *logical_size,
                    EntryNode::Symlink { target_hex } => (target_hex.len() / 2) as u64,
                    EntryNode::Directory { .. } => unreachable!(),
                };
                if size > options.max_new_file_size
                    && !options.force_tracking_matcher.matches(&path)
                {
                    stats.untracked_paths.insert(
                        path,
                        UntrackedReason::FileTooLarge {
                            size,
                            max_size: options.max_new_file_size,
                        },
                    );
                    continue;
                }
            }
        }
        // Validate symlink encoding before writing any private tree identity.
        if let EntryNode::Symlink { target_hex } = &entry.node {
            String::from_utf8(hex::decode(target_hex)?)
                .context("jj symlinks require UTF-8 targets")?;
        }
        selected.insert(path, entry.node);
    }
    Ok(())
}

async fn project_leaf(
    source: &MeteredSource,
    baseline: &MergedTree,
    path: &RepoPath,
    node: &EntryNode,
    old: &MergedTreeValue,
    prior: Option<&ProjectedPath>,
) -> Result<(MergedTreeValue, Option<usize>)> {
    let store = baseline.store();
    match node {
        EntryNode::File {
            object,
            logical_size,
            mode,
        } => {
            let executable = mode & 0o111 != 0;
            if old.as_resolved().is_none() {
                let file_ids = old
                    .to_file_merge()
                    .context("unsupported non-file conflict")?;
                // Preflight every side before jj's helper reads full blobs.
                preflight_conflict(source, store, &file_ids)?;
                let bytes = source.read_all_file(object, *logical_size)?;
                let marker_len = if let Some(length) = prior.and_then(|p| p.conflict_marker_len) {
                    length
                } else {
                    let materialized =
                        materialize_conflict(source, baseline, path, old, None).await?;
                    ensure!(
                        materialized.bytes == bytes,
                        "edited conflict lacks its owned materialization receipt; cannot infer prior marker length"
                    );
                    materialized.marker_len
                };
                let merged_exec = conflicts::resolve_file_executable(
                    &old.to_executable_merge()
                        .context("invalid executable conflict")?,
                );
                ensure!(
                    merged_exec == Some(executable),
                    "changing executable mode on an unresolved conflict is unsupported"
                );
                let new_ids =
                    conflicts::update_from_content(&file_ids, store, path, &bytes, marker_len)
                        .await?;
                if let Some(id) = new_ids.as_resolved() {
                    let copy_id = old
                        .to_copy_id_merge()
                        .and_then(|m| m.resolve_trivial(SameChange::Accept).cloned().flatten())
                        .unwrap_or_else(CopyId::placeholder);
                    return Ok((
                        Merge::resolved(id.clone().map(|id| TreeValue::File {
                            id,
                            executable,
                            copy_id,
                        })),
                        None,
                    ));
                }
                ensure!(
                    new_ids.iter().count() == old.iter().count(),
                    "jj conflict update changed term arity"
                );
                return Ok((old.with_new_file_ids(&new_ids), Some(marker_len)));
            }
            let id = projection_io::write_file(source, store, object, *logical_size)?;
            let copy_id = match old.as_resolved().and_then(Option::as_ref) {
                Some(TreeValue::File { copy_id, .. }) => copy_id.clone(),
                _ => CopyId::placeholder(),
            };
            Ok((
                Merge::resolved(Some(TreeValue::File {
                    id,
                    executable,
                    copy_id,
                })),
                None,
            ))
        }
        EntryNode::Symlink { target_hex } => {
            ensure!(
                old.as_resolved().is_some(),
                "replacing an unresolved file conflict with a symlink is unsupported"
            );
            let target = String::from_utf8(hex::decode(target_hex)?)?;
            let id = store.write_symlink(path, &target).await?;
            Ok((Merge::resolved(Some(TreeValue::Symlink(id))), None))
        }
        EntryNode::Directory { .. } => bail!("directory is not a leaf"),
    }
}

fn preflight_conflict(
    source: &MeteredSource,
    store: &jj_lib::store::Store,
    ids: &Merge<Option<jj_lib::backend::FileId>>,
) -> Result<()> {
    for id in ids.iter().flatten() {
        // Header admission happens before find_object allocates. The returned
        // bytes are discarded here; jj subsequently reads its own immutable
        // blobs. Count both bounded copies in the materialization budget.
        projection_io::read_git_blob(source, store, id.as_bytes(), 2)?;
    }
    Ok(())
}
struct MaterializedConflict {
    bytes: Vec<u8>,
    marker_len: usize,
    executable: bool,
}
async fn materialize_conflict(
    source: &MeteredSource,
    tree: &MergedTree,
    path: &RepoPath,
    value: &MergedTreeValue,
    marker: Option<usize>,
) -> Result<MaterializedConflict> {
    let ids = value
        .to_file_merge()
        .context("unsupported non-file conflict")?;
    preflight_conflict(source, tree.store(), &ids)?;
    let file =
        conflicts::try_materialize_file_conflict_value(tree.store(), path, value, tree.labels())
            .await?
            .context("unsupported conflict kind")?;
    let marker_len = marker
        .unwrap_or_else(|| conflicts::choose_materialized_conflict_marker_len(&file.contents));
    ensure!(
        (conflicts::MIN_CONFLICT_MARKER_LEN..=4096).contains(&marker_len),
        "conflict marker length exceeds supported profile"
    );
    let bytes = conflicts::materialize_merge_result_to_bytes(
        &file.contents,
        &file.labels,
        &ConflictMaterializeOptions {
            marker_style: ConflictMarkerStyle::Diff,
            marker_len: Some(marker_len),
            merge: tree.store().merge_options().clone(),
        },
    );
    source.admit_jj_buffer(bytes.len() as u64, 1)?;
    Ok(MaterializedConflict {
        bytes: bytes.to_vec(),
        marker_len,
        executable: file
            .executable
            .context("ambiguous conflict executable mode is unsupported")?,
    })
}

/// Materialize an immutable CAS source Directory, not a live checkout. The
/// caller owns staging and must retain the returned root before ending it.
/// Git retains its conflict terms/labels/copy IDs; CAS contains the rendered
/// bytes plus this exact marker receipt. Modes normalize to Git's 644/755.
pub async fn materialize_source(
    store: LocalStore,
    staging_id: &str,
    tree: &MergedTree,
    projection_policy: &ObjectRef,
    previous: Option<&ProjectionRecord>,
    budget: ProjectionBudget,
) -> Result<(ProjectionRecord, ProjectionWork)> {
    let source = MeteredSource::new(store.clone(), budget)?;
    let entries = read_jj_entries(&source, tree)?;
    let content = StagedContentStore::new(&store, staging_id)?;
    let mut root = OutputDirectory::default();
    let mut paths = BTreeMap::new();
    for (path, value) in entries {
        let (node, marker) = if let Some(value) = value.as_resolved() {
            match value {
                Some(TreeValue::File { id, executable, .. }) => {
                    let bytes =
                        projection_io::read_git_blob(&source, tree.store(), id.as_bytes(), 1)?;
                    let object = content::import_file(&content, &mut bytes.as_slice())?;
                    (
                        EntryNode::File {
                            object,
                            logical_size: bytes.len() as u64,
                            mode: if *executable { 0o755 } else { 0o644 },
                        },
                        None,
                    )
                }
                Some(TreeValue::Symlink(id)) => {
                    let bytes =
                        projection_io::read_git_blob(&source, tree.store(), id.as_bytes(), 1)?;
                    std::str::from_utf8(&bytes).context("jj symlink target must be UTF-8")?;
                    let node = EntryNode::Symlink {
                        target_hex: hex::encode(bytes),
                    };
                    (node, None)
                }
                _ => bail!("unsupported non-leaf jj entry"),
            }
        } else {
            let marker = previous
                .filter(|r| r.tree == JjTreeIdentity::from_jj(tree))
                .and_then(|r| r.paths.get(path.as_internal_file_string()))
                .and_then(|p| p.conflict_marker_len);
            let materialized = materialize_conflict(&source, tree, &path, &value, marker).await?;
            let object = content::import_file(&content, &mut materialized.bytes.as_slice())?;
            (
                EntryNode::File {
                    object,
                    logical_size: materialized.bytes.len() as u64,
                    mode: if materialized.executable {
                        0o755
                    } else {
                        0o644
                    },
                },
                Some(materialized.marker_len),
            )
        };
        root.insert(&path, node.clone())?;
        paths.insert(
            path.as_internal_file_string().into(),
            ProjectedPath {
                source: node,
                conflict_marker_len: marker,
            },
        );
    }
    let adapter = LocalDirectoryStore::new(&store, staging_id);
    let directory = root.write(&PagedDirectory::new(&adapter))?;
    let record = ProjectionRecord {
        format_version: 1,
        directory,
        tree: JjTreeIdentity::from_jj(tree),
        projection_policy: projection_policy.clone(),
        paths,
    };
    record.validate()?;
    Ok((record, source.work()))
}

/// Derive the selected versioned namespace from an owned projection receipt.
/// This writes only new directory metadata; content stays under the retained
/// input observation. The caller must retain this new root before releasing
/// that observation or staging. The return type asserts no writer barrier.
pub fn write_versioned_directory(
    store: &LocalStore,
    staging_id: &str,
    record: &ProjectionRecord,
) -> Result<ObjectRef> {
    record.validate()?;
    let mut root = OutputDirectory::default();
    for (path, entry) in &record.paths {
        root.insert(&checked_path(path)?, entry.source.clone())?;
    }
    root.write(&PagedDirectory::new(&LocalDirectoryStore::new(
        store, staging_id,
    )))
}

#[derive(Default)]
struct OutputDirectory {
    files: BTreeMap<String, EntryNode>,
    directories: BTreeMap<String, OutputDirectory>,
}
impl OutputDirectory {
    fn insert(&mut self, path: &RepoPath, node: EntryNode) -> Result<()> {
        let mut components = path.components().map(|c| c.as_internal_str()).peekable();
        let mut directory = self;
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                ensure!(
                    !directory.directories.contains_key(component),
                    "jj source file/directory collision"
                );
                directory.files.insert(component.into(), node);
                return Ok(());
            }
            ensure!(
                !directory.files.contains_key(component),
                "jj source file/directory collision"
            );
            directory = directory.directories.entry(component.into()).or_default();
        }
        bail!("root is not a source leaf")
    }
    fn write<S: directory::DirectoryStore + ?Sized>(
        self,
        pages: &PagedDirectory<'_, S>,
    ) -> Result<ObjectRef> {
        let mut entries = Vec::new();
        for (name, node) in self.files {
            entries.push(DirectoryEntry::new(name.as_bytes(), node)?);
        }
        for (name, directory) in self.directories {
            entries.push(DirectoryEntry::new(
                name.as_bytes(),
                EntryNode::Directory {
                    object: directory.write(pages)?,
                },
            )?);
        }
        pages.build(0o755, entries)
    }
}
