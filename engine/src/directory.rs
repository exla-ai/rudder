//! Immutable byte-radix directory pages; this is not the default importer/VFS.
//!
//! Directory schema 2 points at shard schema 1. The latter fixes the layout:
//! maximal raw-byte common prefixes, sorted terminator/byte edges, and a leaf
//! exactly when canonical entry bytes total <=60 KiB and entry count <=1024.
//! Every actual encoded page is additionally limited to 64 KiB. Those rules
//! make a directory's identity independent of insertion/deletion history.
//! Updates read a radix path. Deletion may coalesce only a subtree whose entry
//! summaries prove it fits one bounded leaf; it never scans a large directory.
//!
//! Page-local validation is order-independent for uploads. Publication must
//! additionally validate every parent/child edge with `validate_child`, or use
//! `validate_directory` for this directory's complete shard namespace. Nested
//! directory and file references belong to the outer typed closure traversal.
//! Mutation callers hold staging retention until the new root is published.
//! Readers also hold a retained root or live staging lease; an unprotected
//! partial transfer is not a stable snapshot against concurrent CAS eviction.

use crate::protocol::{canonical_json, HashAlgorithm, ObjectKind, ObjectRef};
use crate::store::LocalStore;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const DIRECTORY_FORMAT_VERSION: u32 = 2;
pub const SHARD_FORMAT_VERSION: u32 = 1;
pub const MAX_PAGE_BYTES: usize = 64 * 1024;
pub const LEAF_ENTRIES_BYTES: u64 = 60 * 1024;
pub const LEAF_MAX_ENTRIES: u64 = 1024;
pub const MAX_NAME_BYTES: usize = 255;
pub const MAX_SYMLINK_BYTES: usize = 4096;
pub const MAX_SCAN_ENTRIES: usize = 1024;
const MAX_ROOT_BYTES: usize = 4096;
pub const MAX_OPERATION_PAGES: u64 = 8192;
pub const MAX_PATH_DEPTH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryRoot {
    pub format_version: u32,
    pub mode: u32,
    pub entry_count: u64,
    pub subdirectory_count: u64,
    pub directory_shard: ObjectRef,
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
        /// File references address descriptors; this is the actual file length.
        logical_size: u64,
        mode: u32,
    },
    Directory {
        object: ObjectRef,
    },
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
        let name = raw_hex(&self.name_hex, MAX_NAME_BYTES)?;
        valid_name(&name)?;
        Ok(name)
    }
    pub fn validate(&self) -> Result<()> {
        self.name()?;
        match &self.node {
            EntryNode::File {
                object,
                logical_size,
                mode,
            } => {
                object.validate()?;
                ensure!(
                    matches!(object.kind, ObjectKind::Blob | ObjectKind::File),
                    "file entry requires Blob or File"
                );
                ensure!(
                    *logical_size <= crate::content::MAX_LOGICAL_BYTES,
                    "file entry exceeds supported logical size"
                );
                valid_mode(*mode)?;
                if object.kind == ObjectKind::Blob {
                    ensure!(
                        *logical_size == object.size_bytes,
                        "blob logical size differs from object length"
                    );
                } else {
                    ensure!(
                        object.size_bytes > 0 && object.size_bytes <= MAX_PAGE_BYTES as u64,
                        "file descriptor exceeds metadata bound"
                    );
                }
            }
            EntryNode::Directory { object } => expect_kind(object, ObjectKind::Directory)?,
            EntryNode::Symlink { target_hex } => {
                let target = raw_hex(target_hex, MAX_SYMLINK_BYTES)?;
                ensure!(
                    !target.is_empty() && !target.contains(&0),
                    "invalid symlink text"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub entry_count: u64,
    pub subdirectory_count: u64,
    /// Sum of canonical serialized entry lengths, excluding array commas.
    pub entries_bytes: u64,
}
impl Summary {
    fn validate(self) -> Result<()> {
        ensure!(
            self.subdirectory_count <= self.entry_count,
            "subdirectory count exceeds entries"
        );
        ensure!(
            (self.entry_count == 0) == (self.entries_bytes == 0),
            "empty directory summary mismatch"
        );
        Ok(())
    }
    fn add(self, other: Self) -> Result<Self> {
        Ok(Self {
            entry_count: self
                .entry_count
                .checked_add(other.entry_count)
                .context("entry count overflow")?,
            subdirectory_count: self
                .subdirectory_count
                .checked_add(other.subdirectory_count)
                .context("subdirectory count overflow")?,
            entries_bytes: self
                .entries_bytes
                .checked_add(other.entries_bytes)
                .context("entry byte count overflow")?,
        })
    }
}

/// Compact, schema-fixed SHA256/DirectoryShard link. Keeping kind/algorithm
/// implicit here bounds even a 257-way branch with maximal integer summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardLink {
    pub digest: String,
    pub size_bytes: u64,
    pub summary: Summary,
}
impl ShardLink {
    pub fn reference(&self) -> ObjectRef {
        ObjectRef {
            kind: ObjectKind::DirectoryShard,
            algorithm: HashAlgorithm::Sha256,
            digest: self.digest.clone(),
            size_bytes: self.size_bytes,
        }
    }
    fn validate(&self) -> Result<()> {
        shard_reference(&self.reference())?;
        self.summary.validate()?;
        ensure!(self.summary.entry_count > 0, "branch points to empty child");
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardChild {
    /// 0: a name equal to this prefix; b+1: the next raw byte b.
    pub edge: u16,
    pub shard: ShardLink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShardNode {
    Leaf { entries: Vec<DirectoryEntry> },
    Branch { children: Vec<ShardChild> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryShard {
    pub format_version: u32,
    pub prefix_hex: String,
    pub summary: Summary,
    pub node: ShardNode,
}

/// Implementations acknowledge immutable object bytes, never mutable CAS paths.
/// The caller supplies retention/publication. Every read and write result is
/// independently hash/size checked by this module as well.
pub trait DirectoryStore {
    fn read(&self, object: &ObjectRef) -> Result<Vec<u8>>;
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef>;
}

/// A live staging lease is mandatory for writes; read-only use needs no lease.
pub struct LocalDirectoryStore<'a> {
    store: &'a LocalStore,
    staging_id: Option<&'a str>,
}
impl<'a> LocalDirectoryStore<'a> {
    pub fn new(store: &'a LocalStore, staging_id: &'a str) -> Self {
        Self {
            store,
            staging_id: Some(staging_id),
        }
    }
    pub fn read_only(store: &'a LocalStore) -> Self {
        Self {
            store,
            staging_id: None,
        }
    }
}
impl DirectoryStore for LocalDirectoryStore<'_> {
    fn read(&self, object: &ObjectRef) -> Result<Vec<u8>> {
        self.store.read_object(object)
    }
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        let lease = self
            .staging_id
            .context("directory mutation requires staging retention")?;
        let object = ObjectRef::from_bytes(kind, bytes);
        self.store.receive_staged(lease, &object, bytes)?;
        Ok(object)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanPage {
    pub entries: Vec<DirectoryEntry>,
    /// Exclusive raw-name continuation, bound to the root passed to scan().
    pub next_after: Option<Vec<u8>>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryUpdate {
    pub root: ObjectRef,
    pub shards_read: u64,
    pub shards_written: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryValidation {
    pub summary: Summary,
    pub shards_read: u64,
    /// Direct file/blob/nested-directory refs, not their transitive contents.
    pub references: Vec<ObjectRef>,
}

pub struct PagedDirectory<'a, S: DirectoryStore + ?Sized> {
    store: &'a S,
}
impl<'a, S: DirectoryStore + ?Sized> PagedDirectory<'a, S> {
    pub fn new(store: &'a S) -> Self {
        Self { store }
    }
    pub fn read_root(&self, root: &ObjectRef) -> Result<DirectoryRoot> {
        let mut op = Operation::new(self.store);
        op.root(root)
    }
    pub fn build(
        &self,
        mode: u32,
        entries: impl IntoIterator<Item = DirectoryEntry>,
    ) -> Result<ObjectRef> {
        valid_mode(mode)?;
        let mut entries: Vec<_> = entries.into_iter().collect();
        entries.sort_by(|a, b| a.name_hex.cmp(&b.name_hex));
        entry_summary(&entries)?;
        let mut op = Operation::new(self.store);
        let shard = op.build(entries, 0)?;
        op.put_root(mode, &shard)
    }
    pub fn lookup(&self, root: &ObjectRef, name: &[u8]) -> Result<Option<DirectoryEntry>> {
        valid_name(name)?;
        let mut op = Operation::new(self.store);
        let directory = op.root(root)?;
        op.lookup(
            &directory.directory_shard,
            root_summary(&directory),
            name,
            0,
        )
    }
    pub fn scan(&self, root: &ObjectRef, after: Option<&[u8]>, limit: usize) -> Result<ScanPage> {
        ensure!(
            (1..=MAX_SCAN_ENTRIES).contains(&limit),
            "directory scan limit must be 1..=1024"
        );
        if let Some(after) = after {
            valid_name(after)?;
        }
        let mut op = Operation::new(self.store);
        let directory = op.root(root)?;
        let mut entries = Vec::new();
        op.scan(
            &directory.directory_shard,
            root_summary(&directory),
            after,
            limit + 1,
            &mut entries,
            0,
        )?;
        let more = entries.len() > limit;
        if more {
            entries.truncate(limit);
        }
        let next_after = if more {
            Some(
                entries
                    .last()
                    .context("missing scan continuation")?
                    .name()?,
            )
        } else {
            None
        };
        Ok(ScanPage {
            entries,
            next_after,
        })
    }
    pub fn update(
        &self,
        root: &ObjectRef,
        name: &[u8],
        node: Option<EntryNode>,
    ) -> Result<DirectoryUpdate> {
        valid_name(name)?;
        if let Some(node) = &node {
            DirectoryEntry::new(name, node.clone())?;
        }
        let mut op = Operation::new(self.store);
        let directory = op.root(root)?;
        let shard = op.update(
            &directory.directory_shard,
            root_summary(&directory),
            name,
            node,
            0,
        )?;
        let updated = if shard.reference == directory.directory_shard {
            root.clone()
        } else {
            op.put_root(directory.mode, &shard)?
        };
        Ok(DirectoryUpdate {
            root: updated,
            shards_read: op.reads,
            shards_written: op.writes,
        })
    }
    /// Path components are raw names; symlink traversal and implicit directory
    /// creation are deliberately unsupported. Existing child roots must be v2.
    pub fn update_path(
        &self,
        root: &ObjectRef,
        path: &[Vec<u8>],
        node: Option<EntryNode>,
    ) -> Result<DirectoryUpdate> {
        ensure!(
            !path.is_empty() && path.len() <= MAX_PATH_DEPTH,
            "invalid directory update path depth"
        );
        for name in path {
            valid_name(name)?;
        }
        let mut op = Operation::new(self.store);
        let updated = op.update_path(root, path, node, 0)?;
        Ok(DirectoryUpdate {
            root: updated,
            shards_read: op.reads,
            shards_written: op.writes,
        })
    }
    pub fn with_mode(&self, root: &ObjectRef, mode: u32) -> Result<ObjectRef> {
        valid_mode(mode)?;
        let mut op = Operation::new(self.store);
        let mut directory = op.root(root)?;
        if directory.mode == mode {
            return Ok(root.clone());
        }
        directory.mode = mode;
        op.put(ObjectKind::Directory, &canonical_json(&directory)?)
    }
    /// Validates one directory's shard tree, not nested directories/file data.
    /// Immutable external references are returned to the outer closure walker.
    pub fn validate_directory(
        &self,
        root: &ObjectRef,
        max_shards: u64,
    ) -> Result<DirectoryValidation> {
        ensure!(
            max_shards > 0,
            "directory validation requires a page budget"
        );
        let mut op = Operation::new(self.store);
        op.budget = max_shards;
        let directory = op.root(root)?;
        let mut references = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let summary = op.validate_walk(
            &directory.directory_shard,
            root_summary(&directory),
            None,
            &mut seen,
            &mut references,
            0,
        )?;
        Ok(DirectoryValidation {
            summary,
            shards_read: op.reads,
            references: references.into_iter().collect(),
        })
    }
}

#[derive(Clone, Copy)]
enum ExpectedSummary {
    Root {
        entry_count: u64,
        subdirectory_count: u64,
    },
    Shard(Summary),
}
fn root_summary(root: &DirectoryRoot) -> Option<ExpectedSummary> {
    Some(ExpectedSummary::Root {
        entry_count: root.entry_count,
        subdirectory_count: root.subdirectory_count,
    })
}
fn fits_leaf(summary: Summary) -> bool {
    summary.entries_bytes <= LEAF_ENTRIES_BYTES && summary.entry_count <= LEAF_MAX_ENTRIES
}
fn valid_mode(mode: u32) -> Result<()> {
    ensure!(mode & !0o7777 == 0, "invalid Unix mode");
    Ok(())
}
fn valid_name(name: &[u8]) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= MAX_NAME_BYTES
            && name != b"."
            && name != b".."
            && !name.contains(&0)
            && !name.contains(&b'/'),
        "unsafe or oversized directory name"
    );
    Ok(())
}
fn raw_hex(value: &str, max: usize) -> Result<Vec<u8>> {
    ensure!(
        value.len() <= max * 2
            && value.len() % 2 == 0
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "noncanonical or oversized raw-byte hex"
    );
    Ok(hex::decode(value)?)
}
fn expect_kind(object: &ObjectRef, kind: ObjectKind) -> Result<()> {
    object.validate()?;
    ensure!(object.kind == kind, "wrong directory reference kind");
    Ok(())
}
fn shard_reference(object: &ObjectRef) -> Result<()> {
    expect_kind(object, ObjectKind::DirectoryShard)?;
    ensure!(
        object.size_bytes > 0 && object.size_bytes <= MAX_PAGE_BYTES as u64,
        "directory shard exceeds page bound"
    );
    Ok(())
}
fn entry_summary(entries: &[DirectoryEntry]) -> Result<Summary> {
    let mut summary = Summary::default();
    let mut previous: Option<&str> = None;
    for entry in entries {
        entry.validate()?;
        ensure!(
            previous.is_none_or(|p| p < entry.name_hex.as_str()),
            "directory names must be unique and byte-sorted"
        );
        previous = Some(&entry.name_hex);
        summary = summary.add(Summary {
            entry_count: 1,
            subdirectory_count: u64::from(matches!(entry.node, EntryNode::Directory { .. })),
            entries_bytes: canonical_json(entry)?.len().try_into()?,
        })?;
    }
    Ok(summary)
}
fn common_prefix(entries: &[DirectoryEntry]) -> Result<Vec<u8>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let first = entries[0].name()?;
    let last = entries[entries.len() - 1].name()?;
    let n = first.iter().zip(&last).take_while(|(a, b)| a == b).count();
    Ok(first[..n].to_vec())
}
fn edge(prefix: &[u8], name: &[u8]) -> Result<u16> {
    ensure!(name.starts_with(prefix), "name is outside radix prefix");
    Ok(if name.len() == prefix.len() {
        0
    } else {
        u16::from(name[prefix.len()]) + 1
    })
}
fn canonical_decode<T: for<'de> Deserialize<'de> + Serialize>(
    bytes: &[u8],
    max: usize,
) -> Result<T> {
    ensure!(bytes.len() <= max, "directory metadata exceeds bound");
    let decoded: T = serde_json::from_slice(bytes)?;
    ensure!(
        canonical_json(&decoded)? == bytes,
        "directory metadata must be canonical compact JSON"
    );
    Ok(decoded)
}

pub fn decode_directory_root(bytes: &[u8]) -> Result<DirectoryRoot> {
    let root: DirectoryRoot = canonical_decode(bytes, MAX_ROOT_BYTES)?;
    ensure!(
        root.format_version == DIRECTORY_FORMAT_VERSION,
        "directory requires schema 2"
    );
    valid_mode(root.mode)?;
    ensure!(
        root.subdirectory_count <= root.entry_count,
        "root subdirectory count exceeds entries"
    );
    shard_reference(&root.directory_shard)?;
    Ok(root)
}
pub fn decode_shard(bytes: &[u8]) -> Result<DirectoryShard> {
    let shard: DirectoryShard = canonical_decode(bytes, MAX_PAGE_BYTES)?;
    ensure!(
        shard.format_version == SHARD_FORMAT_VERSION,
        "unsupported directory shard layout"
    );
    let prefix = raw_hex(&shard.prefix_hex, MAX_NAME_BYTES)?;
    ensure!(
        !prefix.contains(&0) && !prefix.contains(&b'/'),
        "unsafe shard prefix"
    );
    shard.summary.validate()?;
    match &shard.node {
        ShardNode::Leaf { entries } => {
            ensure!(
                entries.len() <= LEAF_MAX_ENTRIES as usize,
                "leaf entry count exceeds layout bound"
            );
            let summary = entry_summary(entries)?;
            ensure!(
                summary == shard.summary && fits_leaf(summary),
                "leaf summary or packing mismatch"
            );
            ensure!(
                common_prefix(entries)? == prefix,
                "leaf prefix is not its maximal common prefix"
            );
        }
        ShardNode::Branch { children } => {
            ensure!(
                (2..=257).contains(&children.len()) && prefix.len() < MAX_NAME_BYTES,
                "invalid radix branch fanout/prefix"
            );
            ensure!(
                !fits_leaf(shard.summary),
                "noncanonical branch fits one leaf"
            );
            let mut total = Summary::default();
            let mut previous = None;
            let mut objects = BTreeSet::new();
            for child in children {
                ensure!(
                    child.edge <= 256
                        && !matches!(child.edge, 1 | 48)
                        && previous.is_none_or(|p| p < child.edge),
                    "radix edges must be sorted, unique and safe"
                );
                previous = Some(child.edge);
                child.shard.validate()?;
                ensure!(
                    objects.insert(child.shard.reference()),
                    "branch repeats a shard"
                );
                if child.edge == 0 {
                    ensure!(
                        child.shard.summary.entry_count == 1,
                        "terminator child must contain exactly one name"
                    );
                }
                total = total.add(child.shard.summary)?;
            }
            ensure!(total == shard.summary, "branch summaries do not add up");
        }
    }
    Ok(shard)
}

/// Pure local shape/refs validation for unordered receive_staged. Cross-object
/// routing/count checks are deferred to validate_child at root publication.
pub fn references_from_bytes(kind: ObjectKind, bytes: &[u8]) -> Result<Vec<ObjectRef>> {
    match kind {
        ObjectKind::Directory => Ok(vec![decode_directory_root(bytes)?.directory_shard]),
        ObjectKind::DirectoryShard => {
            let shard = decode_shard(bytes)?;
            Ok(match shard.node {
                ShardNode::Branch { children } => {
                    children.into_iter().map(|c| c.shard.reference()).collect()
                }
                ShardNode::Leaf { entries } => entries
                    .into_iter()
                    .filter_map(|e| match e.node {
                        EntryNode::File { object, .. } | EntryNode::Directory { object } => {
                            Some(object)
                        }
                        EntryNode::Symlink { .. } => None,
                    })
                    .collect(),
            })
        }
        _ => bail!("object is not a directory root/shard"),
    }
}

/// Pure validation under the caller's CAS/GC lock: never calls LocalStore.
/// Blob logical lengths were already checked from their reference, so callers
/// need not reread a large Blob for summary validation. File children are only
/// bounded descriptor bytes; the outer closure separately validates their tree.
pub fn validate_child(
    parent_kind: ObjectKind,
    parent_bytes: &[u8],
    child: &ObjectRef,
    child_bytes: &[u8],
) -> Result<()> {
    child.verify(child_bytes)?;
    match parent_kind {
        ObjectKind::Directory => {
            let root = decode_directory_root(parent_bytes)?;
            ensure!(
                root.directory_shard == *child,
                "root does not reference this shard"
            );
            let shard = decode_shard(child_bytes)?;
            counts_match(root_summary(&root), shard.summary)?;
        }
        ObjectKind::DirectoryShard => {
            let parent = decode_shard(parent_bytes)?;
            match &parent.node {
                ShardNode::Branch { children } => {
                    let link = children
                        .iter()
                        .find(|c| c.shard.reference() == *child)
                        .context("branch does not reference this shard")?;
                    let shard = decode_shard(child_bytes)?;
                    routing(&parent, link, &shard)?;
                }
                ShardNode::Leaf { entries } => {
                    // A leaf can share this File descriptor across many names.
                    // Decode it once, then check every matching entry's length.
                    let child_logical_size = if child.kind == ObjectKind::File {
                        Some(crate::content::decode_file_node(child_bytes)?.logical_size)
                    } else {
                        None
                    };
                    let mut found = false;
                    for entry in entries {
                        match &entry.node {
                            EntryNode::File {
                                object,
                                logical_size,
                                ..
                            } if object == child => {
                                found = true;
                                if let Some(child_logical_size) = child_logical_size {
                                    ensure!(child_logical_size == *logical_size, "file descriptor logical length differs from directory entry");
                                }
                            }
                            EntryNode::Directory { object } if object == child => found = true,
                            _ => {}
                        }
                    }
                    ensure!(found, "leaf does not reference this child");
                }
            }
        }
        _ => bail!("parent is not a directory root/shard"),
    }
    Ok(())
}

fn counts_match(expected: Option<ExpectedSummary>, actual: Summary) -> Result<()> {
    if let Some(expected) = expected {
        let matches = match expected {
            ExpectedSummary::Root {
                entry_count,
                subdirectory_count,
            } => {
                entry_count == actual.entry_count && subdirectory_count == actual.subdirectory_count
            }
            ExpectedSummary::Shard(summary) => summary == actual,
        };
        ensure!(matches, "actual child summary differs from parent");
    }
    Ok(())
}
fn routing(parent: &DirectoryShard, child: &ShardChild, actual: &DirectoryShard) -> Result<()> {
    counts_match(
        Some(ExpectedSummary::Shard(child.shard.summary)),
        actual.summary,
    )?;
    let prefix = raw_hex(&parent.prefix_hex, MAX_NAME_BYTES)?;
    let actual_prefix = raw_hex(&actual.prefix_hex, MAX_NAME_BYTES)?;
    if child.edge == 0 {
        ensure!(
            actual_prefix == prefix,
            "terminator child has another prefix"
        );
        let ShardNode::Leaf { entries } = &actual.node else {
            bail!("terminator child is not a leaf");
        };
        ensure!(
            entries.len() == 1 && entries[0].name()? == prefix,
            "terminator child is not the exact prefix name"
        );
    } else {
        ensure!(
            actual_prefix.starts_with(&prefix)
                && actual_prefix.len() > prefix.len()
                && u16::from(actual_prefix[prefix.len()]) + 1 == child.edge,
            "child prefix lies outside parent edge range"
        );
    }
    Ok(())
}

#[derive(Clone)]
struct Pointer {
    reference: ObjectRef,
    summary: Summary,
}
struct Operation<'a, S: DirectoryStore + ?Sized> {
    store: &'a S,
    reads: u64,
    writes: u64,
    budget: u64,
}
impl<'a, S: DirectoryStore + ?Sized> Operation<'a, S> {
    fn new(store: &'a S) -> Self {
        Self {
            store,
            reads: 0,
            writes: 0,
            budget: MAX_OPERATION_PAGES,
        }
    }
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        let expected = ObjectRef::from_bytes(kind, bytes);
        ensure!(
            self.store.put(kind, bytes)? == expected,
            "directory store acknowledged another object"
        );
        Ok(expected)
    }
    fn root(&mut self, object: &ObjectRef) -> Result<DirectoryRoot> {
        expect_kind(object, ObjectKind::Directory)?;
        ensure!(
            object.size_bytes <= MAX_ROOT_BYTES as u64,
            "directory root exceeds bound; legacy roots require the legacy adapter"
        );
        let bytes = self.store.read(object)?;
        object.verify(&bytes)?;
        decode_directory_root(&bytes)
    }
    fn shard(
        &mut self,
        object: &ObjectRef,
        expected: Option<ExpectedSummary>,
    ) -> Result<DirectoryShard> {
        shard_reference(object)?;
        self.reads = self.reads.checked_add(1).context("page count overflow")?;
        ensure!(
            self.reads <= self.budget,
            "directory operation page budget exceeded"
        );
        let bytes = self.store.read(object)?;
        object.verify(&bytes)?;
        let shard = decode_shard(&bytes)?;
        counts_match(expected, shard.summary)?;
        Ok(shard)
    }
    fn put_shard(&mut self, shard: DirectoryShard) -> Result<Pointer> {
        let bytes = canonical_json(&shard)?;
        decode_shard(&bytes)?;
        let reference = self.put(ObjectKind::DirectoryShard, &bytes)?;
        self.writes = self.writes.checked_add(1).context("page count overflow")?;
        Ok(Pointer {
            reference,
            summary: shard.summary,
        })
    }
    fn put_root(&self, mode: u32, pointer: &Pointer) -> Result<ObjectRef> {
        let root = DirectoryRoot {
            format_version: DIRECTORY_FORMAT_VERSION,
            mode,
            entry_count: pointer.summary.entry_count,
            subdirectory_count: pointer.summary.subdirectory_count,
            directory_shard: pointer.reference.clone(),
        };
        let bytes = canonical_json(&root)?;
        decode_directory_root(&bytes)?;
        self.put(ObjectKind::Directory, &bytes)
    }
    fn build(&mut self, entries: Vec<DirectoryEntry>, depth: usize) -> Result<Pointer> {
        ensure!(depth <= MAX_NAME_BYTES + 1, "radix depth limit");
        let summary = entry_summary(&entries)?;
        let prefix = common_prefix(&entries)?;
        let node = if fits_leaf(summary) {
            ShardNode::Leaf { entries }
        } else {
            let mut groups = BTreeMap::<u16, Vec<DirectoryEntry>>::new();
            for entry in entries {
                groups
                    .entry(edge(&prefix, &entry.name()?)?)
                    .or_default()
                    .push(entry);
            }
            ensure!(groups.len() >= 2, "oversized single directory entry");
            let mut children = Vec::new();
            for (edge, entries) in groups {
                let pointer = self.build(entries, depth + 1)?;
                children.push(link(edge, &pointer));
            }
            ShardNode::Branch { children }
        };
        self.put_shard(DirectoryShard {
            format_version: SHARD_FORMAT_VERSION,
            prefix_hex: hex::encode(prefix),
            summary,
            node,
        })
    }
    fn checked_child(
        &mut self,
        parent: &DirectoryShard,
        child: &ShardChild,
    ) -> Result<DirectoryShard> {
        let shard = self.shard(
            &child.shard.reference(),
            Some(ExpectedSummary::Shard(child.shard.summary)),
        )?;
        routing(parent, child, &shard)?;
        Ok(shard)
    }
    fn lookup(
        &mut self,
        object: &ObjectRef,
        expected: Option<ExpectedSummary>,
        name: &[u8],
        depth: usize,
    ) -> Result<Option<DirectoryEntry>> {
        ensure!(depth <= MAX_NAME_BYTES + 1, "radix depth limit");
        let shard = self.shard(object, expected)?;
        self.lookup_loaded(shard, name, depth)
    }
    fn lookup_loaded(
        &mut self,
        shard: DirectoryShard,
        name: &[u8],
        depth: usize,
    ) -> Result<Option<DirectoryEntry>> {
        ensure!(depth <= MAX_NAME_BYTES + 1, "radix depth limit");
        let prefix = raw_hex(&shard.prefix_hex, MAX_NAME_BYTES)?;
        if !name.starts_with(&prefix) {
            return Ok(None);
        }
        match &shard.node {
            ShardNode::Leaf { entries } => Ok(entries
                .binary_search_by(|e| e.name_hex.as_str().cmp(&hex::encode(name)))
                .ok()
                .map(|i| entries[i].clone())),
            ShardNode::Branch { children } => {
                let route = edge(&prefix, name)?;
                let Ok(i) = children.binary_search_by_key(&route, |c| c.edge) else {
                    return Ok(None);
                };
                let child = self.checked_child(&shard, &children[i])?;
                self.lookup_loaded(child, name, depth + 1)
            }
        }
    }
    fn scan(
        &mut self,
        object: &ObjectRef,
        expected: Option<ExpectedSummary>,
        after: Option<&[u8]>,
        limit: usize,
        entries: &mut Vec<DirectoryEntry>,
        depth: usize,
    ) -> Result<()> {
        let shard = self.shard(object, expected)?;
        self.scan_loaded(shard, after, limit, entries, depth)
    }
    fn scan_loaded(
        &mut self,
        shard: DirectoryShard,
        after: Option<&[u8]>,
        limit: usize,
        entries: &mut Vec<DirectoryEntry>,
        depth: usize,
    ) -> Result<()> {
        ensure!(depth <= MAX_NAME_BYTES + 1, "radix depth limit");
        let prefix = raw_hex(&shard.prefix_hex, MAX_NAME_BYTES)?;
        if after.is_some_and(|after| before(&prefix, after)) {
            return Ok(());
        }
        match &shard.node {
            ShardNode::Leaf { entries: leaf } => {
                let after_hex = after.map(hex::encode);
                for entry in leaf {
                    if after_hex
                        .as_ref()
                        .is_none_or(|after| entry.name_hex.as_str() > after.as_str())
                    {
                        entries.push(entry.clone());
                        if entries.len() == limit {
                            break;
                        }
                    }
                }
            }
            ShardNode::Branch { children } => {
                for child in children {
                    if entries.len() == limit {
                        break;
                    }
                    let mut route = prefix.clone();
                    if child.edge == 0 {
                        if after.is_some_and(|after| route.as_slice() <= after) {
                            continue;
                        }
                    } else {
                        route.push((child.edge - 1) as u8);
                        if after.is_some_and(|after| before(&route, after)) {
                            continue;
                        }
                    }
                    let actual = self.checked_child(&shard, child)?;
                    self.scan_loaded(actual, after, limit, entries, depth + 1)?;
                }
            }
        }
        Ok(())
    }
    fn update(
        &mut self,
        object: &ObjectRef,
        expected: Option<ExpectedSummary>,
        name: &[u8],
        node: Option<EntryNode>,
        depth: usize,
    ) -> Result<Pointer> {
        let shard = self.shard(object, expected)?;
        self.update_loaded(object, shard, name, node, depth)
    }
    fn update_loaded(
        &mut self,
        object: &ObjectRef,
        shard: DirectoryShard,
        name: &[u8],
        node: Option<EntryNode>,
        depth: usize,
    ) -> Result<Pointer> {
        ensure!(depth <= MAX_NAME_BYTES + 1, "radix depth limit");
        let unchanged = Pointer {
            reference: object.clone(),
            summary: shard.summary,
        };
        let prefix = raw_hex(&shard.prefix_hex, MAX_NAME_BYTES)?;
        if !name.starts_with(&prefix) {
            let Some(node) = node else {
                return Ok(unchanged);
            };
            let entry = DirectoryEntry::new(name, node)?;
            let n = prefix.iter().zip(name).take_while(|(a, b)| a == b).count();
            let shared = prefix[..n].to_vec();
            let added = self.build(vec![entry], depth + 1)?;
            let mut children = vec![
                link(edge(&shared, &prefix)?, &unchanged),
                link(edge(&shared, name)?, &added),
            ];
            children.sort_by_key(|c| c.edge);
            return self.branch(shared, children, depth);
        }
        match &shard.node {
            ShardNode::Leaf { entries } => {
                let mut entries = entries.clone();
                let encoded = hex::encode(name);
                match (entries.binary_search_by(|e| e.name_hex.cmp(&encoded)), node) {
                    (Ok(i), Some(node)) if entries[i].node == node => return Ok(unchanged),
                    (Ok(i), Some(node)) => entries[i] = DirectoryEntry::new(name, node)?,
                    (Ok(i), None) => {
                        entries.remove(i);
                    }
                    (Err(i), Some(node)) => entries.insert(i, DirectoryEntry::new(name, node)?),
                    (Err(_), None) => return Ok(unchanged),
                }
                self.build(entries, depth)
            }
            ShardNode::Branch { children } => {
                let mut children = children.clone();
                let route = edge(&prefix, name)?;
                match children.binary_search_by_key(&route, |c| c.edge) {
                    Ok(i) => {
                        let child = self.checked_child(&shard, &children[i])?;
                        let old = children[i].shard.reference();
                        let result = self.update_loaded(&old, child, name, node, depth + 1)?;
                        if result.reference == old {
                            return Ok(unchanged);
                        }
                        if result.summary.entry_count == 0 {
                            children.remove(i);
                        } else {
                            children[i] = link(route, &result);
                        }
                    }
                    Err(i) => {
                        let Some(node) = node else {
                            return Ok(unchanged);
                        };
                        let added =
                            self.build(vec![DirectoryEntry::new(name, node)?], depth + 1)?;
                        children.insert(i, link(route, &added));
                    }
                }
                self.branch(prefix, children, depth)
            }
        }
    }
    fn branch(
        &mut self,
        prefix: Vec<u8>,
        children: Vec<ShardChild>,
        depth: usize,
    ) -> Result<Pointer> {
        if children.is_empty() {
            return self.build(Vec::new(), depth);
        }
        if children.len() == 1 {
            return Ok(Pointer {
                reference: children[0].shard.reference(),
                summary: children[0].shard.summary,
            });
        }
        let summary = children.iter().try_fold(Summary::default(), |sum, child| {
            sum.add(child.shard.summary)
        })?;
        let shard = DirectoryShard {
            format_version: SHARD_FORMAT_VERSION,
            prefix_hex: hex::encode(prefix),
            summary,
            node: ShardNode::Branch { children },
        };
        if fits_leaf(summary) {
            let mut entries = Vec::new();
            let ShardNode::Branch { children } = &shard.node else {
                unreachable!()
            };
            for child in children {
                let actual = self.checked_child(&shard, child)?;
                self.collect_small(actual, &mut entries, depth + 1)?;
            }
            ensure!(
                entry_summary(&entries)? == summary,
                "coalescing subtree summary mismatch"
            );
            self.build(entries, depth)
        } else {
            self.put_shard(shard)
        }
    }
    fn collect_small(
        &mut self,
        shard: DirectoryShard,
        entries: &mut Vec<DirectoryEntry>,
        depth: usize,
    ) -> Result<()> {
        ensure!(
            depth <= MAX_NAME_BYTES + 1
                && shard.summary.entries_bytes <= LEAF_ENTRIES_BYTES
                && shard.summary.entry_count <= LEAF_MAX_ENTRIES,
            "coalescing exceeds bounded leaf"
        );
        match &shard.node {
            ShardNode::Leaf { entries: leaf } => entries.extend(leaf.iter().cloned()),
            ShardNode::Branch { children } => {
                for child in children {
                    let actual = self.checked_child(&shard, child)?;
                    self.collect_small(actual, entries, depth + 1)?;
                }
            }
        }
        ensure!(
            entries.len() <= LEAF_MAX_ENTRIES as usize,
            "coalescing entry bound"
        );
        Ok(())
    }
    fn update_path(
        &mut self,
        object: &ObjectRef,
        path: &[Vec<u8>],
        node: Option<EntryNode>,
        depth: usize,
    ) -> Result<ObjectRef> {
        ensure!(depth < MAX_PATH_DEPTH, "directory path depth limit");
        let directory = self.root(object)?;
        let node = if path.len() == 1 {
            node
        } else {
            let found = self
                .lookup(
                    &directory.directory_shard,
                    root_summary(&directory),
                    &path[0],
                    0,
                )?
                .context("directory path component missing")?;
            let EntryNode::Directory { object } = found.node else {
                bail!("directory update path crosses a file or symlink");
            };
            Some(EntryNode::Directory {
                object: self.update_path(&object, &path[1..], node, depth + 1)?,
            })
        };
        let updated = self.update(
            &directory.directory_shard,
            root_summary(&directory),
            &path[0],
            node,
            0,
        )?;
        if updated.reference == directory.directory_shard {
            Ok(object.clone())
        } else {
            self.put_root(directory.mode, &updated)
        }
    }
    fn validate_walk(
        &mut self,
        object: &ObjectRef,
        expected: Option<ExpectedSummary>,
        parent: Option<(&DirectoryShard, &ShardChild)>,
        seen: &mut BTreeSet<ObjectRef>,
        references: &mut BTreeSet<ObjectRef>,
        depth: usize,
    ) -> Result<Summary> {
        ensure!(
            depth <= MAX_NAME_BYTES + 1 && seen.insert(object.clone()),
            "directory shard cycle/alias/depth violation"
        );
        let shard = self.shard(object, expected)?;
        if let Some((parent, child)) = parent {
            routing(parent, child, &shard)?;
        }
        match &shard.node {
            ShardNode::Leaf { entries } => {
                for entry in entries {
                    match &entry.node {
                        EntryNode::File { object, .. } | EntryNode::Directory { object } => {
                            references.insert(object.clone());
                        }
                        EntryNode::Symlink { .. } => {}
                    }
                }
            }
            ShardNode::Branch { children } => {
                for child in children {
                    self.validate_walk(
                        &child.shard.reference(),
                        Some(ExpectedSummary::Shard(child.shard.summary)),
                        Some((&shard, child)),
                        seen,
                        references,
                        depth + 1,
                    )?;
                }
            }
        }
        Ok(shard.summary)
    }
}
fn link(edge: u16, pointer: &Pointer) -> ShardChild {
    ShardChild {
        edge,
        shard: ShardLink {
            digest: pointer.reference.digest.clone(),
            size_bytes: pointer.reference.size_bytes,
            summary: pointer.summary,
        },
    }
}
fn before(prefix: &[u8], after: &[u8]) -> bool {
    prefix < after && !after.starts_with(prefix)
}
