//! Immutable, checksum-verified local content and persistent source trees.
//!
//! Acknowledged writes sync the file and containing directory on this volume.
//! This is local durability, not replication. Retention transactions are atomic
//! within this store and are not atomic with the engine's separate journal.
//! Import requires the caller to quiesce writers (including descendants); the
//! store neither stops processes nor claims to snapshot a changing filesystem.
//! Export is eager, into a fresh exclusively owned directory, without hardlinks
//! to the CAS. Tree updates copy only metadata along the changed path.

use crate::protocol::{
    canonical_json, CheckpointConsistency, Durability, ObjectKind, ObjectRef, FORMAT_VERSION,
};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::{
    ffi::{OsStrExt, OsStringExt},
    fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryManifest {
    pub format_version: u32,
    pub mode: u32,
    /// Sorted by decoded raw filename bytes; no duplicate names.
    pub entries: Vec<DirectoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryEntry {
    pub name_hex: String,
    pub node: EntryNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntryNode {
    File {
        object: ObjectRef,
        mode: u32,
    },
    Directory {
        object: ObjectRef,
    },
    /// Raw link text. It may be relative or absolute; it is never dereferenced.
    Symlink {
        target_hex: String,
    },
}

impl DirectoryEntry {
    pub fn new(name: &[u8], node: EntryNode) -> Result<Self> {
        let entry = Self {
            name_hex: hex::encode(name),
            node,
        };
        entry.validate()?;
        Ok(entry)
    }

    pub fn name(&self) -> Result<Vec<u8>> {
        decode_name(&self.name_hex)
    }

    pub fn validate(&self) -> Result<()> {
        self.name()?;
        match &self.node {
            EntryNode::File { object, mode } => {
                expect_kind(object, ObjectKind::Blob)?;
                validate_mode(*mode)?;
            }
            EntryNode::Directory { object } => expect_kind(object, ObjectKind::Directory)?,
            EntryNode::Symlink { target_hex } => {
                let target = decode_hex(target_hex)?;
                ensure!(
                    !target.is_empty() && !target.contains(&0),
                    "invalid symlink target"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    pub format_version: u32,
    pub directory: ObjectRef,
    pub parents: Vec<ObjectRef>,
    pub consistency: CheckpointConsistency,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentManifest {
    pub format_version: u32,
    pub directory: ObjectRef,
    pub platform: BTreeMap<String, String>,
    pub installer_inputs: Vec<ObjectRef>,
    pub provenance: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointManifest {
    pub format_version: u32,
    pub source: ObjectRef,
    pub environment: Option<ObjectRef>,
    pub parents: Vec<ObjectRef>,
    pub consistency: CheckpointConsistency,
    pub durability: Durability,
    pub knowledge_cursor: u64,
}

/// Workspace schema 2 retains the complete private view independently of its
/// semantic source/environment partition. VCS-local bytes are recovery data;
/// the VCS bridge must rebind workspace metadata when moving machines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointV2Manifest {
    pub format_version: u32,
    pub source: ObjectRef,
    pub environment: Option<ObjectRef>,
    pub parents: Vec<ObjectRef>,
    pub consistency: CheckpointConsistency,
    pub durability: Durability,
    pub knowledge_cursor: u64,
    pub view_root: ObjectRef,
    pub profile: ObjectRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointV3Manifest {
    pub format_version: u32,
    pub source: ObjectRef,
    pub environment: Option<ObjectRef>,
    pub parents: Vec<ObjectRef>,
    pub consistency: CheckpointConsistency,
    pub durability: Durability,
    pub knowledge_cursor: u64,
    pub view_root: ObjectRef,
    pub profile: ObjectRef,
    /// Canonical raw-path hardlink groups. Identity is separate from content
    /// equality; the VFS validates its topology before restoring any links.
    pub hardlinks: ObjectRef,
}

/// Action adapters must declare every referenced CAS object here. Payload is
/// canonical application metadata; it is not an alternate untracked CAS index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferencedMetadata {
    pub format_version: u32,
    pub references: Vec<ObjectRef>,
    pub payload: serde_json::Value,
}

/// Explicit paths relative to the imported root. Each component is raw bytes.
/// Exclusions omit complete subtrees. Default imports everything, including .git.
#[derive(Debug, Clone, Default)]
pub struct ImportPolicy {
    pub excluded_paths: Vec<Vec<Vec<u8>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagingLease {
    pub id: String,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionState {
    pub format_version: u32,
    pub generation: u64,
    pub pins: BTreeMap<String, ObjectRef>,
    pub staging: BTreeMap<String, StagingLease>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateEnvelope {
    state: RetentionState,
    checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcReport {
    pub generation: u64,
    pub retained_objects: usize,
    pub removed_objects: usize,
    pub removed_bytes: u64,
    pub blocked_by_staging: bool,
}

#[derive(Debug, Clone)]
pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        ensure!(
            cfg!(unix),
            "local store currently requires Unix filesystem semantics"
        );
        let absolute = if root.as_ref().is_absolute() {
            root.as_ref().to_path_buf()
        } else {
            std::env::current_dir()?.join(root.as_ref())
        };
        create_directory_all_durable(&absolute)?;
        ensure_directory(root.as_ref())?;
        #[cfg(unix)]
        fs::set_permissions(root.as_ref(), fs::Permissions::from_mode(0o700))?;
        let store = Self {
            root: root.as_ref().canonicalize()?,
        };
        for name in ["objects", "tmp"] {
            let directory = store.root.join(name);
            create_directory_all_durable(&directory)?;
            ensure_directory(&directory)?;
        }
        let _lock = store.lock()?;
        let state_path = store.root.join("retention.json");
        if state_path.try_exists()? {
            store.read_state()?;
        } else {
            ensure!(
                fs::read_dir(store.root.join("objects"))?.next().is_none(),
                "retention state missing from nonempty store; refusing unsafe recovery"
            );
            store.write_state(&RetentionState {
                format_version: FORMAT_VERSION,
                generation: 0,
                pins: BTreeMap::new(),
                staging: BTreeMap::new(),
            })?;
        }
        sync_directory(&store.root)?;
        if let Some(parent) = store.root.parent() {
            sync_directory(parent)?;
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path is for diagnostics. Never mutate or hardlink a CAS object into
    /// a writable workspace; use verified reads or an adapter's private CoW.
    pub fn object_path(&self, reference: &ObjectRef) -> Result<PathBuf> {
        reference.validate()?;
        Ok(self
            .root
            .join("objects")
            .join(&reference.digest[..2])
            .join(&reference.digest[2..]))
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<ObjectRef> {
        let reference = ObjectRef::from_bytes(ObjectKind::Blob, bytes);
        self.receive(&reference, bytes)?;
        Ok(reference)
    }

    pub fn put_metadata<T: Serialize>(&self, kind: ObjectKind, metadata: &T) -> Result<ObjectRef> {
        ensure!(kind != ObjectKind::Blob, "use put_blob for raw blobs");
        let bytes = canonical_json(metadata)?;
        let reference = ObjectRef::from_bytes(kind, &bytes);
        self.receive(&reference, &bytes)?;
        Ok(reference)
    }

    pub fn put_directory(&self, mut directory: DirectoryManifest) -> Result<ObjectRef> {
        // Hex byte order is the same as decoded byte order for canonical hex.
        directory
            .entries
            .sort_by(|a, b| a.name_hex.cmp(&b.name_hex));
        self.put_metadata(ObjectKind::Directory, &directory)
    }

    pub fn put_source(&self, source: &SourceManifest) -> Result<ObjectRef> {
        self.put_metadata(ObjectKind::SourceRoot, source)
    }

    pub fn put_environment(&self, environment: &EnvironmentManifest) -> Result<ObjectRef> {
        self.put_metadata(ObjectKind::EnvironmentRoot, environment)
    }

    pub fn put_checkpoint(&self, checkpoint: &CheckpointManifest) -> Result<ObjectRef> {
        ensure!(
            checkpoint.durability == Durability::Local,
            "a local store cannot assert replicated checkpoint durability"
        );
        self.put_metadata(ObjectKind::Checkpoint, checkpoint)
    }

    pub fn put_checkpoint_v2(&self, checkpoint: &CheckpointV2Manifest) -> Result<ObjectRef> {
        ensure!(checkpoint.format_version == 2, "workspace checkpoint requires schema 2");
        ensure!(checkpoint.durability == Durability::Local, "a local store cannot assert replicated checkpoint durability");
        self.put_metadata(ObjectKind::Checkpoint, checkpoint)
    }

    pub fn put_checkpoint_v3(&self, checkpoint: &CheckpointV3Manifest) -> Result<ObjectRef> {
        ensure!(checkpoint.format_version == 3, "VFS checkpoint requires schema 3");
        ensure!(checkpoint.durability == Durability::Local, "a local store cannot assert replicated checkpoint durability");
        self.put_metadata(ObjectKind::Checkpoint, checkpoint)
    }

    /// Receive an independently addressed object, verifying bytes and metadata
    /// schema before publication. Referenced children may still be transferring;
    /// retaining/publishing a root validates the complete reachable closure.
    pub fn receive(&self, reference: &ObjectRef, bytes: &[u8]) -> Result<()> {
        reference.verify(bytes)?;
        references_from_bytes(reference.kind, bytes)?;
        let _lock = self.lock()?;
        self.put_locked(reference, bytes)
    }

    /// Receive into a live transfer lease under the same lock used by GC.
    /// Expired/abandoned staging never silently acquires fresh authority. The
    /// final root publication still verifies and durably retains its closure.
    pub fn receive_staged(&self, staging_id: &str, reference: &ObjectRef, bytes: &[u8]) -> Result<()> {
        reference.verify(bytes)?;
        references_from_bytes(reference.kind, bytes)?;
        let _lock = self.lock()?;
        let state = self.read_state()?;
        let lease = state.staging.get(staging_id).context("unknown staging lease")?;
        ensure!(lease.expires_at_ms > now_ms()?, "staging lease expired");
        self.put_locked(reference, bytes)?;
        ensure!(lease.expires_at_ms > now_ms()?, "staging lease expired during receive");
        Ok(())
    }

    pub fn read_object(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        let _lock = self.lock()?;
        self.read_locked(reference)
    }

    pub fn read_directory(&self, reference: &ObjectRef) -> Result<DirectoryManifest> {
        let _lock = self.lock()?;
        self.directory_locked(reference)
    }

    /// Immediate validated references; use closure() for a transfer inventory.
    pub fn references(&self, reference: &ObjectRef) -> Result<Vec<ObjectRef>> {
        let _lock = self.lock()?;
        references_from_bytes(reference.kind, &self.read_locked(reference)?)
    }

    /// Every returned reference was size/checksum/schema verified while holding
    /// the GC/publication lock. Pin the root or hold a staging lease across a
    /// multi-call transfer: a returned inventory alone is not a retention lease.
    pub fn closure(&self, root: &ObjectRef) -> Result<Vec<ObjectRef>> {
        let _lock = self.lock()?;
        self.closure_locked(root)
    }

    /// Inventory from a currently retained immutable root. Canonical metadata
    /// is verified on this walk; raw blob verification comes from the retained
    /// pin's original transaction. Actual transfer reads still verify bytes.
    /// This is not a fresh integrity scrub after outside CAS mutation.
    pub fn retained_closure(&self, root: &ObjectRef) -> Result<Vec<ObjectRef>> {
        root.validate()?;
        let _lock = self.lock()?;
        let state = self.read_state()?;
        ensure!(state.pins.values().any(|reference| reference == root), "root has no current verified retention authority");
        Ok(self.metadata_closure_locked(std::slice::from_ref(root))?.into_iter().collect())
    }

    pub fn import_tree(
        &self,
        source: impl AsRef<Path>,
        consistency: CheckpointConsistency,
    ) -> Result<ObjectRef> {
        self.import_tree_with_policy(source, consistency, &ImportPolicy::default())
    }

    pub fn import_tree_with_policy(
        &self,
        source: impl AsRef<Path>,
        _consistency: CheckpointConsistency,
        policy: &ImportPolicy,
    ) -> Result<ObjectRef> {
        for excluded in &policy.excluded_paths {
            ensure!(!excluded.is_empty(), "cannot exclude the imported root");
            for component in excluded {
                validate_name(component)?;
            }
        }
        let _lock = self.lock()?;
        self.import_directory_locked(source.as_ref(), &mut Vec::new(), policy)
    }

    /// Eager private materialization; destination must not exist and must be
    /// exclusively owned by the caller. Existing directories are never merged.
    pub fn export_tree(&self, directory: &ObjectRef, destination: impl AsRef<Path>) -> Result<()> {
        let _lock = self.lock()?;
        expect_kind(directory, ObjectKind::Directory)?;
        self.closure_locked(directory)?;
        let destination = destination.as_ref();
        fs::create_dir(destination).context("materialization destination must be fresh")?;
        let result = self.export_directory_locked(directory, destination);
        if result.is_err() {
            let _ = fs::remove_dir_all(destination);
        }
        result
    }

    /// Replace/delete one path in an immutable tree. Parents must exist and be
    /// directories; symlinks are never traversed. Only the path's metadata nodes
    /// are rewritten; unrelated subtrees retain their exact object references.
    pub fn update_path(
        &self,
        directory: &ObjectRef,
        components: &[Vec<u8>],
        replacement: Option<DirectoryEntry>,
    ) -> Result<ObjectRef> {
        ensure!(
            !components.is_empty(),
            "cannot replace the root through update_path"
        );
        for component in components {
            validate_name(component)?;
        }
        if let Some(entry) = &replacement {
            entry.validate()?;
            ensure!(
                entry.name()? == *components.last().unwrap(),
                "replacement filename differs from update path"
            );
        }
        let _lock = self.lock()?;
        self.update_locked(directory, components, replacement)
    }

    pub fn retention(&self) -> Result<RetentionState> {
        let _lock = self.lock()?;
        self.read_state()
    }

    pub fn begin_staging(&self, ttl: Duration) -> Result<StagingLease> {
        let _lock = self.lock()?;
        let mut state = self.read_state()?;
        let lease = StagingLease {
            id: Uuid::new_v4().to_string(),
            expires_at_ms: lease_deadline(ttl)?,
        };
        state.staging.insert(lease.id.clone(), lease.clone());
        self.commit_state(&mut state)?;
        Ok(lease)
    }

    pub fn renew_staging(&self, id: &str, ttl: Duration) -> Result<StagingLease> {
        let _lock = self.lock()?;
        let mut state = self.read_state()?;
        let lease = state.staging.get_mut(id).context("unknown staging lease")?;
        ensure!(
            lease.expires_at_ms > now_ms()?,
            "staging lease expired; acquire a new lease and revalidate transfer"
        );
        lease.expires_at_ms = lease_deadline(ttl)?;
        let result = lease.clone();
        self.commit_state(&mut state)?;
        Ok(result)
    }

    pub fn abandon_staging(&self, id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut state = self.read_state()?;
        if state.staging.remove(id).is_some() {
            self.commit_state(&mut state)?;
        }
        Ok(())
    }

    pub fn publish(&self, staging_id: &str, pin: &str, root: &ObjectRef) -> Result<u64> {
        validate_pin(pin)?;
        let _lock = self.lock()?;
        let mut state = self.read_state()?;
        let lease = state
            .staging
            .get(staging_id)
            .context("unknown staging lease")?;
        ensure!(lease.expires_at_ms > now_ms()?, "staging lease expired");
        self.durable_closure_locked(root)?;
        ensure!(
            lease.expires_at_ms > now_ms()?,
            "staging lease expired during validation"
        );
        state.pins.insert(pin.to_owned(), root.clone());
        state.staging.remove(staging_id);
        self.commit_state(&mut state)?;
        Ok(state.generation)
    }

    pub fn pin(&self, pin: &str, root: &ObjectRef) -> Result<u64> {
        validate_pin(pin)?;
        let _lock = self.lock()?;
        self.durable_closure_locked(root)?;
        let mut state = self.read_state()?;
        state.pins.insert(pin.to_owned(), root.clone());
        self.commit_state(&mut state)?;
        Ok(state.generation)
    }

    /// Publish new content extending explicitly retained, already-verified
    /// roots. Only immutable retention authority permits skipping old blob
    /// hashing/fsync. Unknown objects still receive ordinary full validation.
    /// This has pin_existing's corruption model; external mutation/deletion of
    /// CAS files violates the store contract and is detected on verified reads.
    pub fn publish_extending(&self, staging_id: &str, pin: &str, root: &ObjectRef, retained_roots: &[ObjectRef]) -> Result<u64> {
        validate_pin(pin)?;
        ensure!(retained_roots.len() <= 1024, "too many publication anchors");
        let _lock = self.lock()?;
        let mut state = self.read_state()?;
        let lease = state.staging.get(staging_id).context("unknown staging lease")?;
        ensure!(lease.expires_at_ms > now_ms()?, "staging lease expired");
        for anchor in retained_roots {
            anchor.validate()?;
            ensure!(state.pins.values().any(|reference| reference == anchor), "publication anchor has no current retention authority");
        }
        let verified = self.metadata_closure_locked(retained_roots)?;
        let mut seen = BTreeSet::new();
        let mut pending = vec![root.clone()];
        let mut shards = BTreeSet::new();
        while let Some(reference) = pending.pop() {
            reference.validate()?;
            if verified.contains(&reference) || !seen.insert(reference.clone()) { continue; }
            let bytes = self.read_locked(&reference)?;
            pending.extend(references_from_bytes(reference.kind, &bytes)?);
            let file = self.object_path(&reference)?;
            open_file(&file, false, false)?.sync_all()?;
            shards.insert(file.parent().unwrap().to_path_buf());
        }
        for shard in shards { sync_directory(&shard)?; }
        sync_directory(&self.root.join("objects"))?;
        ensure!(lease.expires_at_ms > now_ms()?, "staging lease expired during validation");
        state.pins.insert(pin.to_owned(), root.clone());
        state.staging.remove(staging_id);
        self.commit_state(&mut state)?;
        Ok(state.generation)
    }

    /// Fork retention of a currently pinned, previously verified root without
    /// walking its tree. No GC can invalidate that authority while a pin exists.
    /// Work scales with retention-record count, not source-tree file count.
    /// External edits to immutable CAS files are corruption, detected on reads;
    /// this fast path does not rescan all bytes to look for outside tampering.
    pub fn pin_existing(&self, pin: &str, root: &ObjectRef) -> Result<u64> {
        validate_pin(pin)?;
        root.validate()?;
        let _lock = self.lock()?;
        let mut state = self.read_state()?;
        ensure!(
            state.pins.values().any(|reference| reference == root),
            "root has no current verified retention authority"
        );
        state.pins.insert(pin.to_owned(), root.clone());
        self.commit_state(&mut state)?;
        Ok(state.generation)
    }

    pub fn unpin(&self, pin: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut state = self.read_state()?;
        if state.pins.remove(pin).is_some() {
            self.commit_state(&mut state)?;
        }
        Ok(())
    }

    /// Conservative initial GC: active staging freezes all sweeping. One lock
    /// covers mark, validation and sweep, fencing every concurrent publication.
    /// Missing/corrupt pinned closure, unknown files or a corrupt retention state
    /// abort BEFORE any deletion. Grace is based on local object mtime; a clock
    /// rollback retains objects. Stale tmp files are retained for later recovery.
    pub fn gc(&self, grace: Duration) -> Result<GcReport> {
        let _lock = self.lock()?;
        let state = self.read_state()?;
        let mut report = GcReport {
            generation: state.generation,
            retained_objects: 0,
            removed_objects: 0,
            removed_bytes: 0,
            blocked_by_staging: false,
        };
        if state
            .staging
            .values()
            .any(|lease| lease.expires_at_ms > now_ms().unwrap_or(0))
        {
            report.blocked_by_staging = true;
            return Ok(report);
        }
        let mut retained = BTreeSet::new();
        for root in state.pins.values() {
            for reference in self.closure_locked(root)? {
                retained.insert(reference.digest);
            }
        }
        report.retained_objects = retained.len();
        let mut sweep = Vec::new();
        for shard in fs::read_dir(self.root.join("objects"))? {
            let shard = shard?;
            ensure!(shard.file_type()?.is_dir(), "unexpected object shard type");
            let shard_name = shard
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("invalid shard name"))?;
            ensure!(
                is_lower_hex(&shard_name) && shard_name.len() == 2,
                "invalid object shard name"
            );
            for item in fs::read_dir(shard.path())? {
                let item = item?;
                ensure!(item.file_type()?.is_file(), "unexpected CAS object type");
                let tail = item
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid object name"))?;
                let digest = format!("{shard_name}{tail}");
                ensure!(
                    digest.len() == 64 && is_lower_hex(&digest),
                    "invalid object filename"
                );
                let bytes = read_regular(&item.path())?;
                ensure!(
                    ObjectRef::from_bytes(ObjectKind::Blob, &bytes).digest == digest,
                    "corrupt object blocks GC"
                );
                if !retained.contains(&digest)
                    && item
                        .metadata()?
                        .modified()?
                        .elapsed()
                        .is_ok_and(|age| age >= grace)
                {
                    sweep.push((item.path(), bytes.len() as u64));
                }
            }
        }
        for (file, size) in sweep {
            fs::remove_file(&file)?;
            sync_directory(file.parent().unwrap())?;
            report.removed_objects += 1;
            report.removed_bytes += size;
        }
        Ok(report)
    }

    fn lock(&self) -> Result<File> {
        ensure_directory(&self.root)?;
        for name in ["objects", "tmp"] {
            ensure_directory(&self.root.join(name))?;
        }
        let lock = open_file(&self.root.join("lock"), true, true)?;
        lock.lock_exclusive()?;
        Ok(lock) // Closing the descriptor releases its cross-process flock.
    }

    fn put_locked(&self, reference: &ObjectRef, bytes: &[u8]) -> Result<()> {
        let destination = self.object_path(reference)?;
        let shard = destination.parent().unwrap();
        if !shard.try_exists()? {
            fs::create_dir(shard)?;
            sync_directory(&self.root.join("objects"))?;
        }
        ensure_directory(shard)?;
        if destination.symlink_metadata().is_ok() {
            ensure!(
                self.read_locked(reference)? == bytes,
                "existing CAS object is corrupt"
            );
            open_file(&destination, false, false)?.sync_all()?;
            sync_directory(shard)?;
            return Ok(());
        }
        let temporary = self.root.join("tmp").join(Uuid::new_v4().to_string());
        let result = (|| -> Result<()> {
            let mut file = create_private(&temporary)?;
            file.write_all(bytes)?;
            #[cfg(unix)]
            file.set_permissions(fs::Permissions::from_mode(0o444))?;
            file.sync_all()?;
            // hard_link publishes without replacing any existing object. This
            // link exists only inside the CAS, never in a writable workspace.
            fs::hard_link(&temporary, &destination)?;
            sync_directory(shard)?;
            fs::remove_file(&temporary)?;
            sync_directory(&self.root.join("tmp"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn read_locked(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        let file = self.object_path(reference)?;
        ensure_directory(file.parent().unwrap())?;
        let bytes = read_regular(&file)
            .with_context(|| format!("reading {:?} {}", reference.kind, reference.digest))?;
        reference.verify(&bytes)?;
        references_from_bytes(reference.kind, &bytes)?;
        Ok(bytes)
    }

    fn directory_locked(&self, reference: &ObjectRef) -> Result<DirectoryManifest> {
        expect_kind(reference, ObjectKind::Directory)?;
        Ok(serde_json::from_slice(&self.read_locked(reference)?)?)
    }

    fn closure_locked(&self, root: &ObjectRef) -> Result<Vec<ObjectRef>> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![root.clone()];
        while let Some(reference) = pending.pop() {
            if !seen.insert(reference.clone()) {
                continue;
            }
            let bytes = self.read_locked(&reference)?;
            pending.extend(references_from_bytes(reference.kind, &bytes)?);
        }
        Ok(seen.into_iter().collect())
    }

    fn metadata_closure_locked(&self, roots: &[ObjectRef]) -> Result<BTreeSet<ObjectRef>> {
        let mut seen = BTreeSet::new();
        let mut pending = roots.to_vec();
        while let Some(reference) = pending.pop() {
            reference.validate()?;
            if !seen.insert(reference.clone()) { continue; }
            if reference.kind != ObjectKind::Blob {
                pending.extend(references_from_bytes(reference.kind, &self.read_locked(&reference)?)?);
            }
        }
        Ok(seen)
    }

    fn durable_closure_locked(&self, root: &ObjectRef) -> Result<()> {
        let mut shards = BTreeSet::new();
        for reference in self.closure_locked(root)? {
            let file = self.object_path(&reference)?;
            open_file(&file, false, false)?.sync_all()?;
            shards.insert(file.parent().unwrap().to_path_buf());
        }
        for shard in shards {
            sync_directory(&shard)?;
        }
        sync_directory(&self.root.join("objects"))?;
        Ok(())
    }

    #[cfg(unix)]
    fn import_directory_locked(
        &self,
        source: &Path,
        relative: &mut Vec<Vec<u8>>,
        policy: &ImportPolicy,
    ) -> Result<ObjectRef> {
        let before = fs::symlink_metadata(source)?;
        ensure!(
            before.is_dir() && !before.file_type().is_symlink(),
            "tree import requires a real directory"
        );
        let mut children: Vec<_> = fs::read_dir(source)?.collect::<std::io::Result<_>>()?;
        children.sort_by(|a, b| a.file_name().as_bytes().cmp(b.file_name().as_bytes()));
        let mut entries = Vec::new();
        for child in children {
            let name = child.file_name().as_bytes().to_vec();
            validate_name(&name)?;
            relative.push(name.clone());
            if policy
                .excluded_paths
                .iter()
                .any(|excluded| relative.starts_with(excluded))
            {
                relative.pop();
                continue;
            }
            let metadata = fs::symlink_metadata(child.path())?;
            let node = if metadata.file_type().is_symlink() {
                EntryNode::Symlink {
                    target_hex: hex::encode(fs::read_link(child.path())?.as_os_str().as_bytes()),
                }
            } else if metadata.is_dir() {
                EntryNode::Directory {
                    object: self.import_directory_locked(&child.path(), relative, policy)?,
                }
            } else if metadata.is_file() {
                let bytes = read_regular(&child.path())?;
                ensure!(
                    same_file(&metadata, &fs::symlink_metadata(child.path())?),
                    "source changed during quiesced import"
                );
                let object = ObjectRef::from_bytes(ObjectKind::Blob, &bytes);
                self.put_locked(&object, &bytes)?;
                EntryNode::File {
                    object,
                    mode: metadata.mode() & 0o7777,
                }
            } else {
                bail!("unsupported source entry type (sockets, devices and FIFOs are not source files)");
            };
            relative.pop();
            entries.push(DirectoryEntry::new(&name, node)?);
        }
        ensure!(
            same_file(&before, &fs::symlink_metadata(source)?),
            "directory changed during quiesced import"
        );
        self.put_directory_locked(&DirectoryManifest {
            format_version: FORMAT_VERSION,
            mode: before.mode() & 0o7777,
            entries,
        })
    }

    #[cfg(not(unix))]
    fn import_directory_locked(
        &self,
        _: &Path,
        _: &mut Vec<Vec<u8>>,
        _: &ImportPolicy,
    ) -> Result<ObjectRef> {
        bail!("raw-byte/mode preserving import requires Unix")
    }

    #[cfg(unix)]
    fn export_directory_locked(&self, reference: &ObjectRef, destination: &Path) -> Result<()> {
        let directory = self.directory_locked(reference)?;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
        for entry in directory.entries {
            let target = destination.join(std::ffi::OsString::from_vec(entry.name()?));
            match entry.node {
                EntryNode::File { object, mode } => {
                    let mut file = create_private(&target)?;
                    file.write_all(&self.read_locked(&object)?)?;
                    file.set_permissions(fs::Permissions::from_mode(mode))?;
                    file.sync_all()?;
                }
                EntryNode::Directory { object } => {
                    fs::create_dir(&target)?;
                    self.export_directory_locked(&object, &target)?;
                }
                EntryNode::Symlink { target_hex } => std::os::unix::fs::symlink(
                    std::ffi::OsString::from_vec(decode_hex(&target_hex)?),
                    &target,
                )?,
            }
        }
        fs::set_permissions(destination, fs::Permissions::from_mode(directory.mode))?;
        sync_directory(destination)?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn export_directory_locked(&self, _: &ObjectRef, _: &Path) -> Result<()> {
        bail!("raw-byte/mode preserving export requires Unix")
    }

    fn put_directory_locked(&self, directory: &DirectoryManifest) -> Result<ObjectRef> {
        let bytes = canonical_json(directory)?;
        let reference = ObjectRef::from_bytes(ObjectKind::Directory, &bytes);
        references_from_bytes(reference.kind, &bytes)?;
        self.put_locked(&reference, &bytes)?;
        Ok(reference)
    }

    fn update_locked(
        &self,
        base: &ObjectRef,
        path: &[Vec<u8>],
        replacement: Option<DirectoryEntry>,
    ) -> Result<ObjectRef> {
        let mut directory = self.directory_locked(base)?;
        let encoded = hex::encode(&path[0]);
        let position = directory
            .entries
            .binary_search_by(|entry| entry.name_hex.cmp(&encoded));
        if path.len() == 1 {
            match (position, replacement) {
                (Ok(index), Some(entry)) => directory.entries[index] = entry,
                (Ok(index), None) => {
                    directory.entries.remove(index);
                }
                (Err(index), Some(entry)) => directory.entries.insert(index, entry),
                (Err(_), None) => return Ok(base.clone()),
            }
        } else {
            let index = position.map_err(|_| anyhow::anyhow!("update parent does not exist"))?;
            let EntryNode::Directory { object } = &directory.entries[index].node else {
                bail!("update parent is not a directory; symlinks are not followed");
            };
            let child = self.update_locked(object, &path[1..], replacement)?;
            directory.entries[index].node = EntryNode::Directory { object: child };
        }
        self.put_directory_locked(&directory)
    }

    fn read_state(&self) -> Result<RetentionState> {
        let envelope: StateEnvelope =
            serde_json::from_slice(&read_regular(&self.root.join("retention.json"))?)?;
        let state_bytes = canonical_json(&envelope.state)?;
        ensure!(
            ObjectRef::from_bytes(ObjectKind::Blob, &state_bytes).digest == envelope.checksum,
            "retention state checksum mismatch; GC/publication disabled"
        );
        ensure!(
            envelope.state.format_version == FORMAT_VERSION,
            "unsupported retention format"
        );
        for (pin, reference) in &envelope.state.pins {
            validate_pin(pin)?;
            reference.validate()?;
        }
        for (id, lease) in &envelope.state.staging {
            ensure!(*id == lease.id && !id.is_empty(), "invalid staging lease");
        }
        Ok(envelope.state)
    }

    fn commit_state(&self, state: &mut RetentionState) -> Result<()> {
        state.generation = state
            .generation
            .checked_add(1)
            .context("retention generation exhausted")?;
        self.write_state(state)
    }

    fn write_state(&self, state: &RetentionState) -> Result<()> {
        let checksum = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(state)?).digest;
        let bytes = canonical_json(&StateEnvelope {
            state: state.clone(),
            checksum,
        })?;
        let temporary = self.root.join("tmp").join(Uuid::new_v4().to_string());
        let result = (|| -> Result<()> {
            let mut file = create_private(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, self.root.join("retention.json"))?;
            sync_directory(&self.root)?;
            sync_directory(&self.root.join("tmp"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn expect_kind(reference: &ObjectRef, kind: ObjectKind) -> Result<()> {
    reference.validate()?;
    ensure!(
        reference.kind == kind,
        "object kind mismatch: expected {kind:?}"
    );
    Ok(())
}

fn validate_mode(mode: u32) -> Result<()> {
    ensure!(mode & !0o7777 == 0, "invalid Unix permission mode");
    Ok(())
}
fn is_lower_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn decode_hex(value: &str) -> Result<Vec<u8>> {
    ensure!(
        value.len() % 2 == 0 && (value.is_empty() || is_lower_hex(value)),
        "byte encoding must be lowercase hexadecimal"
    );
    Ok(hex::decode(value)?)
}
fn decode_name(value: &str) -> Result<Vec<u8>> {
    let name = decode_hex(value)?;
    validate_name(&name)?;
    Ok(name)
}
fn validate_name(name: &[u8]) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name != b"."
            && name != b".."
            && !name.contains(&0)
            && !name.contains(&b'/'),
        "unsafe tree entry name"
    );
    Ok(())
}
fn validate_pin(pin: &str) -> Result<()> {
    ensure!(
        !pin.trim().is_empty() && pin.len() <= 4096 && !pin.contains('\0'),
        "invalid retention pin name"
    );
    Ok(())
}
fn now_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
fn lease_deadline(ttl: Duration) -> Result<u64> {
    let ms: u64 = ttl.as_millis().try_into()?;
    ensure!(ms > 0, "staging TTL must be positive");
    now_ms()?.checked_add(ms).context("staging TTL overflow")
}

fn references_from_bytes(kind: ObjectKind, bytes: &[u8]) -> Result<Vec<ObjectRef>> {
    if kind == ObjectKind::Blob {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    ensure!(
        canonical_json(&value)? == bytes,
        "metadata must use canonical compact JSON"
    );
    let mut references = Vec::new();
    let version;
    match kind {
        ObjectKind::Directory => {
            let directory: DirectoryManifest = serde_json::from_value(value)?;
            version = directory.format_version;
            validate_mode(directory.mode)?;
            let mut previous: Option<Vec<u8>> = None;
            for entry in directory.entries {
                entry.validate()?;
                let name = entry.name()?;
                ensure!(
                    previous.as_ref().is_none_or(|last| last < &name),
                    "directory names must be unique and byte-sorted"
                );
                previous = Some(name);
                match entry.node {
                    EntryNode::File { object, .. } | EntryNode::Directory { object } => {
                        references.push(object)
                    }
                    EntryNode::Symlink { .. } => {}
                }
            }
        }
        ObjectKind::SourceRoot => {
            let source: SourceManifest = serde_json::from_value(value)?;
            version = source.format_version;
            expect_kind(&source.directory, ObjectKind::Directory)?;
            for parent in &source.parents {
                expect_kind(parent, ObjectKind::SourceRoot)?;
            }
            references.push(source.directory);
            references.extend(source.parents);
        }
        ObjectKind::EnvironmentRoot => {
            let environment: EnvironmentManifest = serde_json::from_value(value)?;
            version = environment.format_version;
            expect_kind(&environment.directory, ObjectKind::Directory)?;
            ensure!(
                !environment.platform.is_empty(),
                "environment platform/ABI identity is required"
            );
            references.push(environment.directory);
            references.extend(environment.installer_inputs);
        }
        ObjectKind::Checkpoint => {
            let checkpoint: CheckpointManifest = if value.get("format_version").and_then(serde_json::Value::as_u64) == Some(3) {
                let checkpoint: CheckpointV3Manifest = serde_json::from_value(value)?;
                expect_kind(&checkpoint.view_root, ObjectKind::Directory)?;
                expect_kind(&checkpoint.profile, ObjectKind::Blob)?;
                expect_kind(&checkpoint.hardlinks, ObjectKind::Blob)?;
                references.extend([checkpoint.view_root, checkpoint.profile, checkpoint.hardlinks]);
                CheckpointManifest {
                    format_version: checkpoint.format_version, source: checkpoint.source,
                    environment: checkpoint.environment, parents: checkpoint.parents,
                    consistency: checkpoint.consistency, durability: checkpoint.durability,
                    knowledge_cursor: checkpoint.knowledge_cursor,
                }
            } else if value.get("format_version").and_then(serde_json::Value::as_u64) == Some(2) {
                let checkpoint: CheckpointV2Manifest = serde_json::from_value(value)?;
                expect_kind(&checkpoint.view_root, ObjectKind::Directory)?;
                expect_kind(&checkpoint.profile, ObjectKind::Blob)?;
                references.push(checkpoint.view_root);
                references.push(checkpoint.profile);
                CheckpointManifest {
                    format_version: checkpoint.format_version, source: checkpoint.source,
                    environment: checkpoint.environment, parents: checkpoint.parents,
                    consistency: checkpoint.consistency, durability: checkpoint.durability,
                    knowledge_cursor: checkpoint.knowledge_cursor,
                }
            } else {
                serde_json::from_value(value)?
            };
            version = checkpoint.format_version;
            expect_kind(&checkpoint.source, ObjectKind::SourceRoot)?;
            references.push(checkpoint.source);
            if let Some(environment) = checkpoint.environment {
                expect_kind(&environment, ObjectKind::EnvironmentRoot)?;
                references.push(environment);
            }
            for parent in &checkpoint.parents {
                expect_kind(parent, ObjectKind::Checkpoint)?;
            }
            references.extend(checkpoint.parents);
        }
        ObjectKind::ActionKey | ObjectKind::ActionResult => {
            let action: ReferencedMetadata = serde_json::from_value(value)?;
            version = action.format_version;
            references = action.references;
        }
        ObjectKind::Blob => unreachable!(),
    }
    ensure!(
        version == FORMAT_VERSION || (kind == ObjectKind::Checkpoint && (version == 2 || version == 3)),
        "unsupported metadata format version"
    );
    for reference in &references {
        reference.validate()?;
    }
    Ok(references)
}

fn ensure_directory(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.is_dir(),
        "store directory is missing or is a symlink"
    );
    Ok(())
}
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
fn create_directory_all_durable(path: &Path) -> Result<()> {
    if path.try_exists()? {
        ensure!(path.is_dir(), "store ancestor is not a directory");
        return Ok(());
    }
    let parent = path.parent().context("store directory has no parent")?;
    create_directory_all_durable(parent)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => ensure_directory(path)?,
        Err(error) => return Err(error.into()),
    }
    sync_directory(path)?;
    sync_directory(parent)?;
    Ok(())
}
fn open_file(path: &Path, write: bool, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(write).create(create);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path)?;
    ensure!(file.metadata()?.is_file(), "expected regular file");
    Ok(file)
}
fn create_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    Ok(options.open(path)?)
}
fn read_regular(path: &Path) -> Result<Vec<u8>> {
    let mut file = open_file(path, false, false)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}
#[cfg(unix)]
fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.mode() == after.mode()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}
