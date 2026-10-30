//! Current runnable state has a bounded content closure independent of history.
//!
//! Existing SourceRoot/EnvironmentRoot v1 and Checkpoint v1–3 retain their old
//! strong edges. New launch roots require ancestry-free v2 descriptors. History
//! authority retains earlier launch roots, evidence and VCS seeds separately.
//! These validators prove bytes/schema, never that a writer barrier occurred.
//! Local/replicated durability belongs to a publication receipt, not this hash.

use crate::protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef};
use crate::store::LocalStore;
use anyhow::{bail, ensure, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_POLICY_BYTES: u64 = 1024 * 1024;
pub const MAX_TOPOLOGY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceV2 {
    pub format_version: u32,
    pub consistency: CheckpointConsistency,
    pub directory: ObjectRef,
    pub projection_policy: ObjectRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentV2 {
    pub format_version: u32,
    pub directory: ObjectRef,
    pub platform: BTreeMap<String, String>,
    /// Content roots only. Historical install evidence lives in the history
    /// journal and cannot make a new worker download prior workspace ancestry.
    pub installer_inputs: Vec<ObjectRef>,
    pub provenance: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnableRoot {
    pub format_version: u32,
    pub consistency: CheckpointConsistency,
    pub source: ObjectRef,
    pub environment: Option<ObjectRef>,
    pub view_root: ObjectRef,
    pub profile: ObjectRef,
    pub hardlinks: ObjectRef,
    pub knowledge_cursor: u64,
}

fn kind(reference: &ObjectRef, expected: ObjectKind) -> Result<()> {
    reference.validate()?;
    ensure!(reference.kind == expected, "incorrect runnable-state reference kind");
    Ok(())
}
fn blob(reference: &ObjectRef, limit: u64) -> Result<()> {
    kind(reference, ObjectKind::Blob)?;
    ensure!(reference.size_bytes <= limit, "runnable-state blob exceeds its format bound");
    Ok(())
}
fn decode<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T> {
    ensure!(bytes.len() <= MAX_MANIFEST_BYTES, "runnable manifest exceeds64KiB");
    let value: T = serde_json::from_slice(bytes)?;
    ensure!(canonical_json(&value)? == bytes, "runnable metadata must be canonical JSON");
    Ok(value)
}

pub fn decode_source(bytes: &[u8]) -> Result<SourceV2> {
    let source: SourceV2 = decode(bytes)?;
    ensure!(source.format_version == 2, "runnable source requires schema2 without ancestry");
    kind(&source.directory, ObjectKind::Directory)?;
    blob(&source.projection_policy, MAX_POLICY_BYTES)?;
    Ok(source)
}
pub fn decode_environment(bytes: &[u8]) -> Result<EnvironmentV2> {
    let environment: EnvironmentV2 = decode(bytes)?;
    ensure!(environment.format_version == 2, "runnable environment requires schema2");
    ensure!(!environment.platform.is_empty(), "environment platform/ABI identity is required");
    kind(&environment.directory, ObjectKind::Directory)?;
    for reference in &environment.installer_inputs {
        reference.validate()?;
        ensure!(matches!(reference.kind, ObjectKind::Blob | ObjectKind::File | ObjectKind::Directory), "installer inputs must be content roots, without history/evidence edges");
    }
    Ok(environment)
}
pub fn decode_runnable(bytes: &[u8]) -> Result<RunnableRoot> {
    let root: RunnableRoot = decode(bytes)?;
    ensure!(root.format_version == 1, "unsupported runnable-root schema");
    kind(&root.source, ObjectKind::SourceRoot)?;
    if let Some(environment) = &root.environment { kind(environment, ObjectKind::EnvironmentRoot)?; }
    kind(&root.view_root, ObjectKind::Directory)?;
    blob(&root.profile, MAX_POLICY_BYTES)?;
    blob(&root.hardlinks, MAX_TOPOLOGY_BYTES)?;
    Ok(root)
}

pub fn references(kind: ObjectKind, bytes: &[u8]) -> Result<Vec<ObjectRef>> {
    Ok(match kind {
        ObjectKind::SourceRoot => {
            let source = decode_source(bytes)?;
            vec![source.directory, source.projection_policy]
        }
        ObjectKind::EnvironmentRoot => {
            let environment = decode_environment(bytes)?;
            let mut references = vec![environment.directory];
            references.extend(environment.installer_inputs);
            references
        }
        ObjectKind::RunnableRoot => {
            let root = decode_runnable(bytes)?;
            let mut references = vec![root.source, root.view_root, root.profile, root.hardlinks];
            references.extend(root.environment);
            references
        }
        _ => bail!("object is not runnable-state metadata"),
    })
}

/// Actual child versions must be checked even if the child's bytes are already
/// retained. A v1 source/environment can carry a valid but unbounded old history.
/// The caller SHA/size-binds child_bytes to the typed child reference first.
pub fn validate_child(parent_bytes: &[u8], child: &ObjectRef, child_bytes: &[u8]) -> Result<()> {
    let parent = decode_runnable(parent_bytes)?;
    if child == &parent.source { decode_source(child_bytes)?; }
    else if parent.environment.as_ref() == Some(child) { decode_environment(child_bytes)?; }
    else { bail!("child is not the runnable source or environment"); }
    Ok(())
}

/// Explicit staging preserves caller retention across construction/publication.
/// The caller supplies a real quiesced snapshot; this helper creates no barrier.
pub fn stage<T: Serialize>(store: &LocalStore, staging_id: &str, kind: ObjectKind, value: &T) -> Result<ObjectRef> {
    let bytes = canonical_json(value)?;
    references(kind, &bytes)?;
    let reference = ObjectRef::from_bytes(kind, &bytes);
    store.receive_staged(staging_id, &reference, &bytes)?;
    Ok(reference)
}
