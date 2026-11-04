//! Verified partial-tree retention, separate from snapshot/publication authority.
//!
//! Explicit new objects may reference authorized immutable remote boundaries.
//! A plan Blob binds those exact refs, base/attempt and capture identity, and
//! joins the same durable individual-object set. No second commit can lose the
//! plan while keeping its bytes. The owning VFS/journal must separately publish
//! its nonce/head and establish writer quiescence. Nothing here grants that
//! authority or a complete local closure; saved records never restore access.
use crate::{
    content, directory,
    protocol::{canonical_json, ObjectKind, ObjectRef},
    remote_root::{AttemptBinding, ObjectHandle, RemoteObjectPath, RemoteSession, RootBinding},
    store::{metadata_references, LocalStore, ObjectSetReceipt},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct DeltaBudget {
    pub max_objects: usize,
    pub max_boundary_objects: usize,
    pub max_metadata_bytes: u64,
    pub timeout: Duration,
}
impl Default for DeltaBudget {
    fn default() -> Self {
        Self {
            max_objects: 16_000,
            max_boundary_objects: 4096,
            max_metadata_bytes: 64 * 1024 * 1024,
            timeout: Duration::from_secs(30),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaPlan {
    pub format_version: u32,
    pub capture_id: String,
    pub base: RootBinding,
    pub attempt: AttemptBinding,
    pub directory: ObjectRef,
    pub hardlinks: ObjectRef,
    /// Reachable producer-supplied local objects, excluding this plan Blob.
    pub local_objects: Vec<ObjectRef>,
    pub remote_boundaries: Vec<RemoteObjectPath>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialTreeReceipt {
    pub format_version: u32,
    pub plan: ObjectRef,
    pub local: ObjectSetReceipt,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeltaReport {
    pub local_objects: usize,
    pub remote_boundaries: usize,
    pub metadata_reads: u64,
    pub metadata_bytes: u64,
    pub remote_metadata_loads: u64,
    pub validated_edges: u64,
    pub elapsed_ms: u64,
}
/// Data validation/retention only. It is deliberately not a RunnableRoot,
/// quiesced checkpoint, replicated receipt or accepted source-change record.
pub struct RetainedPartialTree {
    plan: DeltaPlan,
    receipt: PartialTreeReceipt,
    report: DeltaReport,
}
impl RetainedPartialTree {
    pub fn plan(&self) -> &DeltaPlan {
        &self.plan
    }
    pub fn receipt(&self) -> &PartialTreeReceipt {
        &self.receipt
    }
    pub fn report(&self) -> &DeltaReport {
        &self.report
    }
}
struct Validation<'a> {
    store: &'a LocalStore,
    session: &'a RemoteSession,
    local: BTreeSet<ObjectRef>,
    boundaries: BTreeMap<ObjectRef, &'a ObjectHandle>,
    used: BTreeSet<ObjectRef>,
    metadata: BTreeMap<ObjectRef, Vec<u8>>,
    report: DeltaReport,
    budget: &'a DeltaBudget,
    deadline: Instant,
}
impl Validation<'_> {
    fn check(&self) -> Result<()> {
        self.session.check_current()?;
        ensure!(
            Instant::now() < self.deadline,
            "partial tree validation deadline exceeded"
        );
        Ok(())
    }
    fn metadata(&mut self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.check()?;
        if let Some(bytes) = self.metadata.get(reference) {
            return Ok(bytes.clone());
        }
        ensure!(
            reference.size_bytes <= 64 * 1024,
            "partial tree metadata requires explicit bounded bulk path"
        );
        self.report.metadata_bytes = self
            .report
            .metadata_bytes
            .checked_add(reference.size_bytes)
            .context("delta metadata byte overflow")?;
        ensure!(
            self.report.metadata_bytes <= self.budget.max_metadata_bytes,
            "partial tree metadata budget exceeded"
        );
        self.report.metadata_reads += 1;
        let bytes = if self.local.contains(reference) {
            self.store.read_object(reference)?
        } else {
            let handle = self
                .boundaries
                .get(reference)
                .context("partial tree has an unproved remote reference")?;
            self.session.validate_handle(handle)?;
            self.used.insert(reference.clone());
            self.report.remote_metadata_loads += 1;
            self.session
                .request(handle, self.deadline)?
                .wait()?
                .with_bytes(|bytes| bytes.to_vec())?
        };
        reference.verify(&bytes)?;
        self.check()?;
        // This byte budget also bounds retained metadata copies. Encoded
        // metadata is never treated as a complete-closure proof cache.
        self.metadata.insert(reference.clone(), bytes.clone());
        Ok(bytes)
    }
    fn edges(&self, reference: &ObjectRef, bytes: &[u8]) -> Result<Vec<ObjectRef>> {
        match reference.kind {
            ObjectKind::Directory => {
                directory::decode_directory_root(bytes)?;
            }
            ObjectKind::DirectoryShard => {
                directory::decode_shard(bytes)?;
            }
            ObjectKind::File => {
                content::decode_file_node(bytes)?;
            }
            _ => bail!("partial tree contains unsupported metadata kind"),
        }
        metadata_references(reference.kind, bytes)
    }
    fn validate_edge(&mut self, parent: &ObjectRef, bytes: &[u8], child: &ObjectRef) -> Result<()> {
        if child.kind == ObjectKind::Blob {
            // Schema validation already binds Blob logical length to its typed
            // ref. A remote boundary needs membership, never chunk hydration.
            if !self.local.contains(child) {
                let handle = self
                    .boundaries
                    .get(child)
                    .context("unproved remote Blob boundary")?;
                self.session.validate_handle(handle)?;
                self.used.insert(child.clone());
            }
            return Ok(());
        }
        let child_bytes = self.metadata(child)?;
        match parent.kind {
            ObjectKind::Directory | ObjectKind::DirectoryShard => {
                directory::validate_child(parent.kind, bytes, child, &child_bytes)?
            }
            ObjectKind::File => {
                let node = content::decode_file_node(bytes)?;
                let sub = content::decode_file_node(&child_bytes)?;
                for (index, reference) in node.children.iter().enumerate() {
                    if reference == child {
                        content::validate_file_child(&node, index, &sub)?;
                    }
                }
            }
            _ => bail!("unsupported partial-tree parent"),
        }
        Ok(())
    }
    fn walk(&mut self, root: &ObjectRef) -> Result<BTreeSet<ObjectRef>> {
        let mut retained = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut pending = vec![root.clone()];
        while let Some(reference) = pending.pop() {
            self.check()?;
            reference.validate()?;
            if !seen.insert(reference.clone()) {
                continue;
            }
            ensure!(
                seen.len() <= self.budget.max_objects,
                "partial tree object budget exceeded"
            );
            if !self.local.contains(&reference) {
                let handle = self.boundaries.get(&reference).context(
                    "partial tree reference is neither produced locally nor rooted remotely",
                )?;
                self.session.validate_handle(handle)?;
                self.used.insert(reference.clone());
                if reference.kind != ObjectKind::Blob {
                    self.metadata(&reference)?;
                }
                continue;
            }
            retained.insert(reference.clone());
            if reference.kind == ObjectKind::Blob {
                continue;
            }
            let bytes = self.metadata(&reference)?;
            // One descriptor can reference the same immutable child many times.
            // validate_edge checks every occurrence in this parent (including
            // each File span/leaf length), so repeating that whole validation
            // per occurrence makes a shared-file leaf need quadratic work.
            // Keep deduplication local to this parent: another parent still
            // has to validate its own routing, summaries and logical lengths.
            let edges: BTreeSet<_> = self.edges(&reference, &bytes)?.into_iter().collect();
            for child in edges {
                self.check()?;
                self.validate_edge(&reference, &bytes, &child)?;
                self.report.validated_edges += 1;
                pending.push(child);
            }
        }
        Ok(retained)
    }
}

/// Blocking bounded validation/metadata fetch/fsync; call on a host I/O worker.
/// The producer retains its staging lease until this succeeds. Live and quiesced
/// captures may use it, but neither classification is inferred or recorded here.
pub fn retain_partial_tree(
    store: &LocalStore,
    staging_id: &str,
    pin_id: &str,
    capture_id: &str,
    session: &RemoteSession,
    root: &ObjectRef,
    hardlinks: &ObjectRef,
    produced: &[ObjectRef],
    remote_boundaries: &[ObjectHandle],
    budget: &DeltaBudget,
) -> Result<RetainedPartialTree> {
    let started = Instant::now();
    ensure!(
        Uuid::parse_str(capture_id).is_ok_and(|id| id.to_string() == capture_id),
        "capture identity must be a canonical UUID"
    );
    ensure!(
        root.kind == ObjectKind::Directory && hardlinks.kind == ObjectKind::Blob,
        "partial tree needs directory/topology refs"
    );
    ensure!(
        budget.max_objects > 0
            && budget.max_objects <= 16_000
            && budget.max_boundary_objects > 0
            && budget.max_boundary_objects <= 4096
            && budget.max_metadata_bytes > 0
            && budget.max_metadata_bytes <= 64 * 1024 * 1024
            && !budget.timeout.is_zero()
            && budget.timeout <= Duration::from_secs(120),
        "invalid partial-tree budget"
    );
    ensure!(
        produced.len() <= budget.max_objects
            && remote_boundaries.len() <= budget.max_boundary_objects,
        "partial-tree admission exceeds limits"
    );
    session.check_current()?;
    let mut local = BTreeSet::new();
    for reference in produced {
        reference.validate()?;
        local.insert(reference.clone());
    }
    let mut boundaries = BTreeMap::new();
    for handle in remote_boundaries {
        session.validate_handle(handle)?;
        boundaries
            .entry(handle.reference().clone())
            .or_insert(handle);
    }
    let mut validation = Validation {
        store,
        session,
        local,
        boundaries,
        used: BTreeSet::new(),
        metadata: BTreeMap::new(),
        report: DeltaReport::default(),
        budget,
        deadline: started
            .checked_add(budget.timeout)
            .context("partial-tree deadline overflow")?,
    };
    let mut retained = validation.walk(root)?;
    if validation.local.contains(hardlinks) {
        retained.insert(hardlinks.clone());
    } else {
        let handle = validation
            .boundaries
            .get(hardlinks)
            .context("topology has no local or remote provenance")?;
        session.validate_handle(handle)?;
        validation.used.insert(hardlinks.clone());
    }
    // First incremental profile supports empty inherited topology. Nonempty
    // topology needs the whole-path/inode verifier and cannot be flattened.
    let topology = validation.metadata(hardlinks)?;
    let decoded: crate::vfs::HardlinkTopology = serde_json::from_slice(&topology)?;
    ensure!(
        decoded.format_version == 1
            && decoded.groups.is_empty()
            && canonical_json(&decoded)? == topology,
        "partial-tree hardlinks need a qualified topology adapter"
    );
    validation.check()?;
    let plan = DeltaPlan {
        format_version: 1,
        capture_id: capture_id.into(),
        base: session.binding().clone(),
        attempt: session.attempt().clone(),
        directory: root.clone(),
        hardlinks: hardlinks.clone(),
        local_objects: retained.iter().cloned().collect(),
        remote_boundaries: validation
            .used
            .iter()
            .map(|reference| validation.boundaries[reference].reference_path())
            .collect::<std::result::Result<Vec<_>, _>>()?,
    };
    let encoded = canonical_json(&plan)?;
    ensure!(
        encoded.len() <= 4 * 1024 * 1024,
        "partial-tree plan exceeds byte limit"
    );
    let plan_ref = ObjectRef::from_bytes(ObjectKind::Blob, &encoded);
    retained.insert(plan_ref.clone());
    let references: Vec<_> = retained.into_iter().collect();
    let local = if let Some(existing) = store.object_set_receipt(pin_id)? {
        ensure!(
            existing.staging_id == staging_id && store.object_set_refs(&existing)? == references,
            "partial-tree pin already binds another capture"
        );
        // Also completes conservative staging cleanup after a lost reply.
        store.retain_object_set(staging_id, pin_id, &references)?
    } else {
        store.receive_staged(staging_id, &plan_ref, &encoded)?;
        validation.check()?;
        store.retain_object_set(staging_id, pin_id, &references)?
    };
    validation.check()?;
    validation.report.local_objects = plan.local_objects.len();
    validation.report.remote_boundaries = plan.remote_boundaries.len();
    validation.report.elapsed_ms = started.elapsed().as_millis().try_into()?;
    Ok(RetainedPartialTree {
        plan,
        receipt: PartialTreeReceipt {
            format_version: 1,
            plan: plan_ref,
            local,
        },
        report: validation.report,
    })
}

/// Read retained data for reconciliation. It does not authorize a new attempt,
/// restore a live proof chain or grant a quiesced/replicated checkpoint.
pub fn read_partial_tree(store: &LocalStore, receipt: &PartialTreeReceipt) -> Result<DeltaPlan> {
    ensure!(
        receipt.format_version == 1
            && receipt.plan.kind == ObjectKind::Blob
            && receipt.plan.size_bytes <= 4 * 1024 * 1024,
        "invalid partial-tree receipt"
    );
    let objects = store.object_set_refs(&receipt.local)?;
    ensure!(
        objects.contains(&receipt.plan),
        "partial-tree plan lacks exact individual retention"
    );
    let bytes = store.read_object(&receipt.plan)?;
    let plan: DeltaPlan = serde_json::from_slice(&bytes)?;
    ensure!(
        plan.format_version == 1 && canonical_json(&plan)? == bytes,
        "partial-tree plan is not canonical"
    );
    let mut expected = plan.local_objects.clone();
    expected.push(receipt.plan.clone());
    expected.sort();
    expected.dedup();
    ensure!(
        objects == expected,
        "partial-tree receipt differs from its retained plan"
    );
    Ok(plan)
}
