//! Explicit V1/V2 directory adapters and quiesced filesystem transfer.
//!
//! This module does not change LocalStore's default importer/materializer. The
//! caller stops every source writer and holds retention while reading. Imports
//! write only through a supplied staging lease; returning two snapshot refs is
//! not publication. Retain BOTH directory and topology before ending staging.
//! File data streams through content; directory metadata has explicit budgets.

use crate::content::{self, StagedContentStore};
use crate::directory::{
    self, DirectoryEntry, EntryNode, LocalDirectoryStore, PagedDirectory, ScanPage,
};
use crate::protocol::{canonical_json, ObjectKind, ObjectRef};
use crate::store::{DirectoryManifest, ImportPolicy, LocalStore};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

const MAX_LEGACY_DIRECTORY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TOPOLOGY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOPOLOGY_PATHS: u64 = 1_000_000;
const MAX_TOPOLOGY_GROUPS: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootInfo {
    pub format_version: u32,
    pub mode: u32,
    pub entry_count: u64,
    pub subdirectory_count: u64,
}

/// Counts expanded namespace work, not unique CAS objects. Logical bytes count
/// every included file path, including hardlinks. Metadata admission includes
/// names, canonical entries and collected raw topology paths; it is not RSS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBudget {
    pub max_entries: u64,
    pub max_logical_bytes: u64,
    pub max_depth: usize,
    pub max_metadata_bytes: u64,
}
impl Default for ImportBudget {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_logical_bytes: 4 * 1024 * 1024 * 1024,
            max_depth: 256,
            max_metadata_bytes: 128 * 1024 * 1024,
        }
    }
}
impl ImportBudget {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.max_depth <= directory::MAX_PATH_DEPTH,
            "tree depth budget exceeds supported path depth"
        );
        ensure!(
            self.max_metadata_bytes > 0,
            "tree metadata budget must be positive"
        );
        Ok(())
    }
}

/// Same wire representation as VfsCheckpoint, without a dependency on VFS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeSnapshot {
    pub format_version: u32,
    pub directory: ObjectRef,
    pub hardlinks: ObjectRef,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardlinkTopology {
    pub format_version: u32,
    pub groups: Vec<HardlinkGroup>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardlinkGroup {
    pub paths_hex: Vec<Vec<String>>,
}

fn name_valid(name: &[u8]) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= directory::MAX_NAME_BYTES
            && name != b"."
            && name != b".."
            && !name.contains(&0)
            && !name.contains(&b'/'),
        "unsafe or oversized tree name"
    );
    Ok(())
}

enum DecodedRoot {
    Legacy(DirectoryManifest),
    Paged(directory::DirectoryRoot),
}
fn decode_root(store: &LocalStore, root: &ObjectRef) -> Result<DecodedRoot> {
    root.validate()?;
    ensure!(
        root.kind == ObjectKind::Directory,
        "tree root must be a directory"
    );
    ensure!(
        root.size_bytes <= MAX_LEGACY_DIRECTORY_BYTES,
        "legacy directory exceeds adapter metadata bound"
    );
    let bytes = store.read_object(root)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    match value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(1) => Ok(DecodedRoot::Legacy(serde_json::from_value(value)?)),
        Some(2) => Ok(DecodedRoot::Paged(directory::decode_directory_root(
            &bytes,
        )?)),
        _ => bail!("unsupported tree directory format"),
    }
}
fn legacy_entry(entry: crate::store::DirectoryEntry) -> Result<DirectoryEntry> {
    let node = match entry.node {
        crate::store::EntryNode::File { object, mode } => EntryNode::File {
            logical_size: object.size_bytes,
            object,
            mode,
        },
        crate::store::EntryNode::Directory { object } => EntryNode::Directory { object },
        crate::store::EntryNode::Symlink { target_hex } => EntryNode::Symlink { target_hex },
    };
    let entry = DirectoryEntry {
        name_hex: entry.name_hex,
        node,
    };
    entry.validate()?;
    Ok(entry)
}

pub fn root_info(store: &LocalStore, root: &ObjectRef) -> Result<RootInfo> {
    Ok(match decode_root(store, root)? {
        DecodedRoot::Legacy(value) => RootInfo {
            format_version: 1,
            mode: value.mode,
            entry_count: value.entries.len().try_into()?,
            subdirectory_count: value
                .entries
                .iter()
                .filter(|entry| matches!(entry.node, crate::store::EntryNode::Directory { .. }))
                .count()
                .try_into()?,
        },
        DecodedRoot::Paged(value) => RootInfo {
            format_version: 2,
            mode: value.mode,
            entry_count: value.entry_count,
            subdirectory_count: value.subdirectory_count,
        },
    })
}

pub fn lookup(store: &LocalStore, root: &ObjectRef, name: &[u8]) -> Result<Option<DirectoryEntry>> {
    name_valid(name)?;
    match decode_root(store, root)? {
        DecodedRoot::Legacy(value) => {
            let key = hex::encode(name);
            value
                .entries
                .binary_search_by(|entry| entry.name_hex.cmp(&key))
                .ok()
                .map(|index| legacy_entry(value.entries[index].clone()))
                .transpose()
        }
        DecodedRoot::Paged(_) => {
            PagedDirectory::new(&LocalDirectoryStore::read_only(store)).lookup(root, name)
        }
    }
}

pub fn scan(
    store: &LocalStore,
    root: &ObjectRef,
    after: Option<&[u8]>,
    limit: usize,
) -> Result<ScanPage> {
    ensure!(
        (1..=directory::MAX_SCAN_ENTRIES).contains(&limit),
        "tree scan limit must be 1..=1024"
    );
    if let Some(after) = after {
        name_valid(after)?;
    }
    match decode_root(store, root)? {
        DecodedRoot::Legacy(value) => {
            let start = after
                .map(|after| {
                    let key = hex::encode(after);
                    value.entries.partition_point(|entry| entry.name_hex <= key)
                })
                .unwrap_or(0);
            let end = start.saturating_add(limit).min(value.entries.len());
            let entries = value.entries[start..end]
                .iter()
                .cloned()
                .map(legacy_entry)
                .collect::<Result<Vec<_>>>()?;
            let next_after = if end < value.entries.len() {
                Some(entries.last().context("missing tree cursor")?.name()?)
            } else {
                None
            };
            Ok(ScanPage {
                entries,
                next_after,
            })
        }
        DecodedRoot::Paged(_) => {
            PagedDirectory::new(&LocalDirectoryStore::read_only(store)).scan(root, after, limit)
        }
    }
}

/// Convert ONLY this immediate directory. Nested directory refs are preserved,
/// allowing an explicit VFS profile to upgrade paths as they are accessed.
pub fn to_paged_directory(
    store: &LocalStore,
    staging_id: &str,
    root: &ObjectRef,
) -> Result<ObjectRef> {
    match decode_root(store, root)? {
        DecodedRoot::Paged(_) => Ok(root.clone()),
        DecodedRoot::Legacy(value) => {
            let entries = value
                .entries
                .into_iter()
                .map(legacy_entry)
                .collect::<Result<Vec<_>>>()?;
            PagedDirectory::new(&LocalDirectoryStore::new(store, staging_id))
                .build(value.mode, entries)
        }
    }
}

type RawPath = Vec<Vec<u8>>;
fn decode_path(path: &[String], budget: &ImportBudget) -> Result<RawPath> {
    ensure!(
        !path.is_empty() && path.len() <= budget.max_depth,
        "invalid topology path depth"
    );
    path.iter()
        .map(|part| {
            ensure!(
                part.len() <= directory::MAX_NAME_BYTES * 2,
                "topology name exceeds bound"
            );
            let bytes = hex::decode(part)?;
            ensure!(
                hex::encode(&bytes) == *part,
                "noncanonical topology name hex"
            );
            name_valid(&bytes)?;
            Ok(bytes)
        })
        .collect()
}
fn file_at(
    store: &LocalStore,
    root: &ObjectRef,
    path: &[Vec<u8>],
) -> Result<(ObjectRef, u64, u32)> {
    let mut directory = root.clone();
    for (index, name) in path.iter().enumerate() {
        let entry = lookup(store, &directory, name)?.context("topology path is absent")?;
        match entry.node {
            EntryNode::File {
                object,
                logical_size,
                mode,
            } if index + 1 == path.len() => {
                ensure!(
                    content::file_info(store, &object)?.logical_size == logical_size,
                    "topology file length differs from content"
                );
                return Ok((object, logical_size, mode));
            }
            EntryNode::Directory { object } if index + 1 < path.len() => directory = object,
            _ => bail!("topology path must traverse directories and end at a regular file"),
        }
    }
    bail!("empty topology file path")
}

/// Checks bounded canonical topology and exact file identity/mode for groups.
/// This does not replace complete CAS closure verification or acquire a pin.
pub fn validate_snapshot(
    store: &LocalStore,
    snapshot: &TreeSnapshot,
    budget: &ImportBudget,
) -> Result<HardlinkTopology> {
    budget.validate()?;
    ensure!(
        snapshot.format_version == 1
            && snapshot.directory.kind == ObjectKind::Directory
            && snapshot.hardlinks.kind == ObjectKind::Blob,
        "unsupported tree snapshot"
    );
    ensure!(
        snapshot.directory.size_bytes <= budget.max_metadata_bytes
            && snapshot.hardlinks.size_bytes <= MAX_TOPOLOGY_BYTES.min(budget.max_metadata_bytes),
        "snapshot metadata exceeds budget"
    );
    root_info(store, &snapshot.directory)?;
    let bytes = store.read_object(&snapshot.hardlinks)?;
    let topology: HardlinkTopology = serde_json::from_slice(&bytes)?;
    ensure!(
        topology.format_version == 1
            && topology.groups.len() <= MAX_TOPOLOGY_GROUPS
            && canonical_json(&topology)? == bytes,
        "invalid or noncanonical hardlink topology"
    );
    let mut seen = BTreeSet::new();
    let mut previous_group: Option<RawPath> = None;
    let mut paths = 0_u64;
    for group in &topology.groups {
        ensure!(
            (2..=100_000).contains(&group.paths_hex.len()),
            "hardlink group size exceeds supported bounds"
        );
        let mut previous: Option<RawPath> = None;
        let mut expected = None;
        for encoded in &group.paths_hex {
            paths = paths
                .checked_add(1)
                .context("topology path count overflow")?;
            ensure!(
                paths <= MAX_TOPOLOGY_PATHS.min(budget.max_entries),
                "topology path count exceeds budget"
            );
            let path = decode_path(encoded, budget)?;
            ensure!(
                previous.as_ref().is_none_or(|last| last < &path) && seen.insert(path.clone()),
                "topology paths are duplicate or unsorted"
            );
            if previous.is_none() {
                ensure!(
                    previous_group.as_ref().is_none_or(|last| last < &path),
                    "topology groups are unsorted"
                );
                previous_group = Some(path.clone());
            }
            let file = file_at(store, &snapshot.directory, &path)?;
            if let Some(expected) = &expected {
                ensure!(*expected == file, "hardlink group content or mode differs");
            } else {
                expected = Some(file);
            }
            previous = Some(path);
        }
    }
    Ok(topology)
}

#[derive(Default)]
struct Usage {
    entries: u64,
    logical: u64,
    metadata: u64,
}
impl Usage {
    fn entry(&mut self, name: &[u8], depth: usize, budget: &ImportBudget) -> Result<()> {
        name_valid(name)?;
        ensure!(depth <= budget.max_depth, "tree path depth exceeds budget");
        self.entries = self
            .entries
            .checked_add(1)
            .context("tree entry count overflow")?;
        ensure!(
            self.entries <= budget.max_entries,
            "tree entry count exceeds budget"
        );
        self.metadata(name.len().try_into()?, budget)
    }
    fn logical(&mut self, bytes: u64, budget: &ImportBudget) -> Result<()> {
        self.logical = self
            .logical
            .checked_add(bytes)
            .context("tree logical byte count overflow")?;
        ensure!(
            self.logical <= budget.max_logical_bytes,
            "tree logical bytes exceed budget"
        );
        Ok(())
    }
    fn metadata(&mut self, bytes: u64, budget: &ImportBudget) -> Result<()> {
        self.metadata = self
            .metadata
            .checked_add(bytes)
            .context("tree metadata count overflow")?;
        ensure!(
            self.metadata <= budget.max_metadata_bytes,
            "tree metadata exceeds budget"
        );
        Ok(())
    }
}

pub fn import_quiesced(
    store: &LocalStore,
    staging_id: &str,
    source: &Path,
    policy: &ImportPolicy,
    budget: &ImportBudget,
) -> Result<TreeSnapshot> {
    budget.validate()?;
    for path in &policy.excluded_paths {
        ensure!(!path.is_empty(), "cannot exclude the imported root");
        for name in path {
            name_valid(name)?;
        }
    }
    #[cfg(unix)]
    {
        filesystem::import(store, staging_id, source, policy, budget)
    }
    #[cfg(not(unix))]
    {
        let _ = (store, staging_id, source, policy);
        bail!("filesystem tree import requires Unix");
    }
}

pub fn export_quiesced(
    store: &LocalStore,
    snapshot: &TreeSnapshot,
    destination: &Path,
    budget: &ImportBudget,
) -> Result<()> {
    let topology = validate_snapshot(store, snapshot, budget)?;
    #[cfg(unix)]
    {
        filesystem::export(store, snapshot, &topology, destination, budget)
    }
    #[cfg(not(unix))]
    {
        let _ = (store, snapshot, topology, destination);
        bail!("filesystem tree export requires Unix");
    }
}

#[cfg(unix)]
mod filesystem {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs::{self, File, OpenOptions};
    use std::io::Read;
    use std::os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    };
    use std::path::PathBuf;

    #[derive(Clone, PartialEq, Eq)]
    struct Stamp {
        device: u64,
        inode: u64,
        mode: u32,
        size: u64,
        links: u64,
        mtime: (i64, i64),
        ctime: (i64, i64),
    }
    impl Stamp {
        fn new(metadata: &fs::Metadata) -> Self {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                mode: metadata.mode(),
                size: metadata.len(),
                links: metadata.nlink(),
                mtime: (metadata.mtime(), metadata.mtime_nsec()),
                ctime: (metadata.ctime(), metadata.ctime_nsec()),
            }
        }
    }
    struct ImportedFile {
        stamp: Stamp,
        content: ObjectRef,
        paths: Vec<RawPath>,
    }
    struct Importer<'a> {
        store: &'a LocalStore,
        stage: &'a str,
        policy: &'a ImportPolicy,
        budget: &'a ImportBudget,
        usage: Usage,
        files: BTreeMap<(u64, u64), ImportedFile>,
    }
    impl Importer<'_> {
        fn directory(&mut self, source: &Path, relative: &mut RawPath) -> Result<ObjectRef> {
            let before = fs::symlink_metadata(source)?;
            ensure!(
                before.is_dir() && !before.file_type().is_symlink(),
                "tree import requires real directories"
            );
            let mut names = Vec::new();
            for item in fs::read_dir(source)? {
                let name = item?.file_name().as_bytes().to_vec();
                self.usage.entry(&name, relative.len() + 1, self.budget)?;
                relative.push(name.clone());
                let excluded = self
                    .policy
                    .excluded_paths
                    .iter()
                    .any(|path| relative.starts_with(path));
                relative.pop();
                if !excluded {
                    names.push(name);
                }
            }
            names.sort();
            let mut entries = Vec::new();
            for name in names {
                relative.push(name.clone());
                let path = source.join(std::ffi::OsString::from_vec(name.clone()));
                let metadata = fs::symlink_metadata(&path)?;
                let node = if metadata.file_type().is_symlink() {
                    let target = fs::read_link(&path)?;
                    ensure!(
                        Stamp::new(&metadata) == Stamp::new(&fs::symlink_metadata(&path)?),
                        "source symlink changed during quiesced import"
                    );
                    EntryNode::Symlink {
                        target_hex: hex::encode(target.as_os_str().as_bytes()),
                    }
                } else if metadata.is_dir() {
                    EntryNode::Directory {
                        object: self.directory(&path, relative)?,
                    }
                } else if metadata.is_file() {
                    self.usage.logical(metadata.len(), self.budget)?;
                    ensure!(
                        metadata.len() <= content::MAX_LOGICAL_BYTES,
                        "source file exceeds supported logical size"
                    );
                    let stamp = Stamp::new(&metadata);
                    let key = (stamp.device, stamp.inode);
                    let content = if let Some(previous) = self.files.get_mut(&key) {
                        ensure!(
                            previous.stamp == stamp,
                            "hardlinked source changed during quiesced import"
                        );
                        previous.paths.push(relative.clone());
                        previous.content.clone()
                    } else {
                        let mut file = OpenOptions::new()
                            .read(true)
                            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                            .open(&path)?;
                        ensure!(
                            file.metadata()?.is_file() && Stamp::new(&file.metadata()?) == stamp,
                            "source file changed before quiesced read"
                        );
                        let maximum = metadata
                            .len()
                            .checked_add(1)
                            .context("source file size overflow")?;
                        let object = content::import_file(
                            &StagedContentStore::new(self.store, self.stage)?,
                            &mut file.by_ref().take(maximum),
                        )?;
                        ensure!(
                            content::file_info(self.store, &object)?.logical_size == metadata.len()
                                && Stamp::new(&file.metadata()?) == stamp
                                && Stamp::new(&fs::symlink_metadata(&path)?) == stamp,
                            "source file changed during quiesced import"
                        );
                        self.files.insert(
                            key,
                            ImportedFile {
                                stamp,
                                content: object.clone(),
                                paths: vec![relative.clone()],
                            },
                        );
                        object
                    };
                    self.usage.metadata(
                        canonical_json(&relative.iter().map(hex::encode).collect::<Vec<_>>())?
                            .len()
                            .try_into()?,
                        self.budget,
                    )?;
                    EntryNode::File {
                        object: content,
                        logical_size: metadata.len(),
                        mode: metadata.mode() & 0o7777,
                    }
                } else {
                    bail!("special files are not supported tree inputs");
                };
                let entry = DirectoryEntry::new(&name, node)?;
                self.usage
                    .metadata(canonical_json(&entry)?.len().try_into()?, self.budget)?;
                entries.push(entry);
                relative.pop();
            }
            ensure!(
                Stamp::new(&before) == Stamp::new(&fs::symlink_metadata(source)?),
                "source directory changed during quiesced import"
            );
            PagedDirectory::new(&LocalDirectoryStore::new(self.store, self.stage))
                .build(before.mode() & 0o7777, entries)
        }
    }

    pub(super) fn import(
        store: &LocalStore,
        stage: &str,
        source: &Path,
        policy: &ImportPolicy,
        budget: &ImportBudget,
    ) -> Result<TreeSnapshot> {
        let metadata = fs::symlink_metadata(source)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "tree source must be a real directory"
        );
        let source = source.canonicalize()?;
        ensure!(
            !source.starts_with(store.root()),
            "cannot import from inside the live CAS"
        );
        if let Ok(relative) = store.root().strip_prefix(&source) {
            let components = relative
                .components()
                .map(|part| part.as_os_str().as_bytes().to_vec())
                .collect::<RawPath>();
            ensure!(
                policy
                    .excluded_paths
                    .iter()
                    .any(|path| components.starts_with(path)),
                "input contains live CAS metadata without an explicit exclusion"
            );
        }
        StagedContentStore::new(store, stage)?;
        let mut importer = Importer {
            store,
            stage,
            policy,
            budget,
            usage: Usage::default(),
            files: BTreeMap::new(),
        };
        let directory = importer.directory(&source, &mut Vec::new())?;
        let mut groups = importer
            .files
            .into_values()
            .filter(|file| file.paths.len() > 1)
            .map(|file| file.paths)
            .collect::<Vec<_>>();
        for paths in &mut groups {
            paths.sort();
        }
        groups.sort();
        ensure!(
            groups.len() <= MAX_TOPOLOGY_GROUPS
                && groups.iter().all(|group| group.len() <= 100_000),
            "hardlink group count exceeds topology bound"
        );
        let topology_paths = groups.iter().try_fold(0_u64, |total, group| {
            total
                .checked_add(group.len().try_into()?)
                .context("topology path count overflow")
        })?;
        ensure!(
            topology_paths <= MAX_TOPOLOGY_PATHS,
            "hardlink path count exceeds topology bound"
        );
        let topology = HardlinkTopology {
            format_version: 1,
            groups: groups
                .into_iter()
                .map(|paths| HardlinkGroup {
                    paths_hex: paths
                        .into_iter()
                        .map(|path| path.iter().map(hex::encode).collect())
                        .collect(),
                })
                .collect(),
        };
        let bytes = canonical_json(&topology)?;
        ensure!(
            bytes.len() as u64 <= MAX_TOPOLOGY_BYTES,
            "hardlink topology exceeds byte bound"
        );
        importer.usage.metadata(bytes.len().try_into()?, budget)?;
        let hardlinks = ObjectRef::from_bytes(ObjectKind::Blob, &bytes);
        store.receive_staged(stage, &hardlinks, &bytes)?;
        Ok(TreeSnapshot {
            format_version: 1,
            directory,
            hardlinks,
        })
    }

    struct Exporter<'a> {
        store: &'a LocalStore,
        budget: &'a ImportBudget,
        usage: Usage,
        membership: BTreeMap<RawPath, usize>,
        first_links: BTreeMap<usize, PathBuf>,
        directories: Vec<(PathBuf, u32)>,
        files: Vec<(PathBuf, u32)>,
        active: BTreeSet<ObjectRef>,
    }
    impl Exporter<'_> {
        fn directory(
            &mut self,
            root: &ObjectRef,
            target: &Path,
            relative: &mut RawPath,
        ) -> Result<()> {
            ensure!(
                relative.len() <= self.budget.max_depth && self.active.insert(root.clone()),
                "tree directory cycle or depth limit"
            );
            ensure!(
                root.size_bytes <= self.budget.max_metadata_bytes,
                "tree directory exceeds metadata budget"
            );
            let info = root_info(self.store, root)?;
            ensure!(
                info.entry_count <= self.budget.max_entries.saturating_sub(self.usage.entries),
                "tree entry count exceeds budget"
            );
            // The root has a cleanup record before any fallible traversal.
            // Update it rather than applying a second final mode afterwards.
            if let Some((_, mode)) = self
                .directories
                .last_mut()
                .filter(|(path, _)| path == target)
            {
                *mode = info.mode;
            } else {
                self.directories.push((target.to_path_buf(), info.mode));
            }
            fs::set_permissions(target, fs::Permissions::from_mode(0o700))?;
            let mut after = None;
            let mut entries = 0_u64;
            let mut subdirectories = 0_u64;
            loop {
                let page = scan(
                    self.store,
                    root,
                    after.as_deref(),
                    directory::MAX_SCAN_ENTRIES,
                )?;
                for entry in page.entries {
                    let name = entry.name()?;
                    self.usage.entry(&name, relative.len() + 1, self.budget)?;
                    self.usage
                        .metadata(canonical_json(&entry)?.len().try_into()?, self.budget)?;
                    entries = entries
                        .checked_add(1)
                        .context("directory entry count overflow")?;
                    relative.push(name.clone());
                    let path = target.join(std::ffi::OsString::from_vec(name));
                    match entry.node {
                        EntryNode::Directory { object } => {
                            subdirectories = subdirectories
                                .checked_add(1)
                                .context("directory count overflow")?;
                            fs::DirBuilder::new().mode(0o700).create(&path)?;
                            // Register before reading child metadata. Even a
                            // restrictive umask or corrupt child must leave a
                            // path cleanup can make accessible and remove.
                            self.directories.push((path.clone(), 0o700));
                            self.directory(&object, &path, relative)?;
                        }
                        EntryNode::File {
                            object,
                            logical_size,
                            mode,
                        } => {
                            self.usage.logical(logical_size, self.budget)?;
                            ensure!(
                                content::file_info(self.store, &object)?.logical_size
                                    == logical_size,
                                "tree file length differs from content"
                            );
                            let group = self.membership.get(relative).copied();
                            if let Some(first) =
                                group.and_then(|group| self.first_links.get(&group))
                            {
                                fs::hard_link(first, &path)?;
                            } else {
                                let mut file = OpenOptions::new()
                                    .write(true)
                                    .create_new(true)
                                    .mode(0o600)
                                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                                    .open(&path)?;
                                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                                ensure!(
                                    content::export_file(self.store, &object, &mut file)?
                                        .logical_size
                                        == logical_size,
                                    "exported file length differs"
                                );
                                file.sync_all()?;
                                self.files.push((path.clone(), mode));
                                if let Some(group) = group {
                                    self.first_links.insert(group, path);
                                }
                            }
                        }
                        EntryNode::Symlink { target_hex } => {
                            symlink(std::ffi::OsString::from_vec(hex::decode(target_hex)?), path)?
                        }
                    }
                    relative.pop();
                }
                after = page.next_after;
                if after.is_none() {
                    break;
                }
            }
            ensure!(
                entries == info.entry_count && subdirectories == info.subdirectory_count,
                "directory summaries differ from exported namespace"
            );
            self.active.remove(root);
            Ok(())
        }
        fn finish(&self) -> Result<()> {
            // Apply final modes only after links are complete: an inherited
            // mode000 file/directory must not block a later hardlink source.
            for (path, mode) in &self.files {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)?;
                file.set_permissions(fs::Permissions::from_mode(*mode))?;
                file.sync_all()?;
            }
            for (path, mode) in self.directories.iter().rev() {
                let directory = File::open(path)?;
                directory.set_permissions(fs::Permissions::from_mode(*mode))?;
                directory.sync_all()?;
            }
            Ok(())
        }
        fn cleanup(&self, root: &Path) -> Result<()> {
            for (path, _) in &self.directories {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
            fs::remove_dir_all(root)?;
            Ok(())
        }
    }

    pub(super) fn export(
        store: &LocalStore,
        snapshot: &TreeSnapshot,
        topology: &HardlinkTopology,
        destination: &Path,
        budget: &ImportBudget,
    ) -> Result<()> {
        let name = destination
            .file_name()
            .context("tree destination requires a fresh directory name")?;
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()?;
        let destination = parent.join(name);
        ensure!(
            !destination.starts_with(store.root()),
            "cannot export inside the live CAS"
        );
        let membership = topology
            .groups
            .iter()
            .enumerate()
            .flat_map(|(index, group)| {
                group
                    .paths_hex
                    .iter()
                    .map(move |path| Ok((decode_path(path, budget)?, index)))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut usage = Usage::default();
        usage.metadata(snapshot.hardlinks.size_bytes, budget)?;
        // Atomic mkdir refuses files, symlinks and preexisting directories.
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&destination)
            .context("tree destination must be fresh")?;
        let mut exporter = Exporter {
            store,
            budget,
            usage,
            membership,
            first_links: BTreeMap::new(),
            directories: vec![(destination.clone(), 0o700)],
            files: Vec::new(),
            active: BTreeSet::new(),
        };
        let result = exporter
            .directory(&snapshot.directory, &destination, &mut Vec::new())
            .and_then(|_| exporter.finish())
            .and_then(|_| {
                File::open(&parent)?.sync_all()?;
                Ok(())
            });
        if let Err(error) = result {
            exporter
                .cleanup(&destination)
                .with_context(|| format!("partial tree export cleanup failed after: {error:#}"))?;
            return Err(error);
        }
        Ok(())
    }
}
