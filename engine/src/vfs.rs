//! Persistent logical workspaces over immutable CAS directories.
//!
//! Legacy preparation builds a shared directory index; the explicit paged/range
//! profile resolves immutable pages directly. Forking stores a root pointer,
//! an owner epoch and an empty private namespace: it never visits inherited
//! entries or hydrates file contents. SQLite FULL transactions acknowledge
//! namespace operations. Legacy writes have ordinary fsync semantics; range
//! logs sync data before their metadata pointer is committed.
//! The local DB is not the project journal or a distributed scheduling authority.
//! No backing files are swept: uncertainty retains data rather than its only copy.

use crate::protocol::{canonical_json, ObjectKind, ObjectRef, OwnerEpoch, FORMAT_VERSION};
use crate::store::{DirectoryEntry, DirectoryManifest, EntryNode, ImportPolicy, LocalStore};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt as LockFileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

mod capture;
mod content;
#[cfg(target_os = "linux")]
pub mod linux;
mod namespace;
mod observation;
mod readdir;
mod remote;
pub mod remote_launch;
pub mod rpc;
pub use capture::{
    PartialCaptureBudget, PartialCaptureReceipt, PartialCaptureRequest, PartialCaptureStatus,
};
pub use observation::{
    exclusion_policy_digest, DirtyCursor, DirtyEvent, DirtyPage, ObservationBudget,
    ObservationConsistency, VfsObservation,
};
pub use readdir::{
    DirectoryEntry as DirectoryListingEntry, DirectoryPage, ReaddirLimits, ReaddirStats,
};
pub use remote::{RemoteOptions, RemoteStats};

const MAX_DEPTH: usize = 1024;
const MAX_IO: usize = 16 * 1024 * 1024;
const MAX_IMMUTABLE_FDS: usize = 128;

#[derive(Clone)]
pub struct VfsManager(Arc<Manager>);
struct Manager {
    root: PathBuf,
    store: LocalStore,
    index: Mutex<Connection>,
    verified: Mutex<(HashMap<ObjectRef, Arc<File>>, VecDeque<ObjectRef>)>,
    remote: Option<Arc<remote::Resolver>>,
}
#[derive(Clone)]
pub struct VfsView(Arc<View>);
struct View {
    manager: VfsManager,
    id: String,
    owner: OwnerEpoch,
    path: PathBuf,
    db: Mutex<Connection>,
    _lock: File,
    mounted: AtomicBool,
    next_handle: AtomicU64,
    handles: Mutex<HashMap<u64, Handle>>,
    pager: Option<Mutex<readdir::Pager>>,
    v2: bool,
}
#[derive(Clone)]
struct Handle {
    ino: u64,
    flags: i32,
    directory: Option<Arc<Vec<(Vec<u8>, VfsAttr)>>>,
    deferred_directory: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Directory,
    Symlink,
}
impl NodeKind {
    fn code(self) -> i64 {
        match self {
            Self::File => 1,
            Self::Directory => 2,
            Self::Symlink => 3,
        }
    }
    fn from_code(v: i64) -> rusqlite::Result<Self> {
        match v {
            1 => Ok(Self::File),
            2 => Ok(Self::Directory),
            3 => Ok(Self::Symlink),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfsAttr {
    pub ino: u64,
    pub kind: NodeKind,
    pub mode: u32,
    pub size: u64,
    pub nlink: u64,
    pub parent: u64,
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfsDescriptor {
    pub format_version: u32,
    pub view_id: String,
    pub owner: OwnerEpoch,
    pub base: ObjectRef,
    pub latest: Option<ObjectRef>,
    pub latest_snapshot: Option<VfsCheckpoint>,
    pub inode_count: u64,
    pub private_dirent_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_plane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_capture_format: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VfsCheckpoint {
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
#[derive(Clone)]
struct Node {
    attr: VfsAttr,
    base: Option<ObjectRef>,
    target: Vec<u8>,
    backing: Option<String>,
    dirty: bool,
    origin: Option<String>,
}

pub fn capabilities() -> serde_json::Value {
    serde_json::json!({
        "format_version":1,"backend":"rudder_logical_vfs","linux_mount":"fuser 0.18 cached write-through",
        "fork":"root pointer, empty private SQLite namespace, retained-root pin; no inherited walk",
        "rename":"transactional parent dirents, stable inode; no descendant walk",
        "runtime_hardlinks":true,"directory_format_v1_inherited_hardlink_identity":false,
        "plain_directory_checkpoint_hardlink_identity":false,"live_checkpoint":false,
        "content_durability":"fsync; namespace commits SQLite FULL; local volume only",
        "optional_data_planes":{
            "paged_ranges_v2":{
                "default":false,"directory_formats":[1,2],"file_formats":["blob","file"],
                "copy_up":"checksummed immutable64KiB private blocks; no inherited file copy",
                "content_durability":"private log data sync before FULL SQLite pointer commit; local only",
                "checkpoint":"dirty radix paths and4MiB chunks; publication still traverses retained metadata",
                "live_observation":"retained LiveBestEffort; acknowledged-generation witness, never quiescence",
                "listing":"opendir currently snapshots a whole directory in memory",
                "mounted_root_replace":false,"default_runtime_switch":false
            },
            "remote_paged_ranges_v1":{
                "default":false,"directory_formats":[2],"file_formats":["blob_le_4MiB","file_v1"],
                "authority":"fresh exact RunnableRoot session; immutable origin proof paths; no arbitrary local-ref fallback",
                "startup":"bounded manifests/profile/empty-topology and root page only",
                "inherited_hardlinks":"nonempty topology requires explicit bulk path; runtime links supported",
                "checkpoint":false,"live_observation":false,"root_pin":"no complete LocalStore pin",
                "partial_capture":"explicit data-only receipt v1; permanent namespace4 activation; nonce/head journal; complete consumers gated",
                "listing":"remote namespace3 fixed snapshots; bounded pages, disk cookies and stable inode reservations; quotas return EAGAIN",
                "default_runtime_switch":false
            }
        },
        "garbage_collection":"no private backing/index sweep; retained CAS roots",
        "mount_provisioning":"root or same-UID user namespace with pre-provisioned device access; direct FD only, no setuid",
        "unsupported":["xattrs","chown","special files","POSIX record locks","live mounted checkpoints","macOS FUSE default"]
    })
}

fn error(errno: i32) -> anyhow::Error {
    std::io::Error::from_raw_os_error(errno).into()
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}
fn name_ok(name: &[u8]) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 255
            && name != b"."
            && name != b".."
            && !name.contains(&0)
            && !name.contains(&b'/'),
        "unsafe filename"
    );
    Ok(())
}
fn secure_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => ensure!(
            m.is_dir() && !m.file_type().is_symlink(),
            "VFS path is not a private directory"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            File::open(path.parent().context("directory parent")?)?.sync_all()?;
        }
        Err(e) => return Err(e.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    File::open(path)?.sync_all()?;
    Ok(())
}
fn connection(path: &Path) -> Result<Connection> {
    if let Ok(m) = fs::symlink_metadata(path) {
        ensure!(
            m.is_file() && !m.file_type().is_symlink(),
            "unsafe database path"
        );
    }
    let c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(10))?;
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(c)
}
fn sql_id(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(-1)
}
fn read_id(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<u64> {
    let v = r.get::<_, i64>(i)?;
    u64::try_from(v).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Integer, Box::new(e))
    })
}
fn json_ref(r: &ObjectRef) -> Result<String> {
    Ok(serde_json::to_string(r)?)
}
fn decode_path(path: &[String]) -> Result<Vec<Vec<u8>>> {
    ensure!(
        !path.is_empty() && path.len() <= MAX_DEPTH,
        "invalid hardlink path depth"
    );
    path.iter()
        .map(|s| {
            let b = hex::decode(s)?;
            ensure!(hex::encode(&b) == *s, "noncanonical name hex");
            name_ok(&b)?;
            Ok(b)
        })
        .collect()
}
fn encode_path(path: &[Vec<u8>]) -> String {
    path.iter().map(hex::encode).collect::<Vec<_>>().join("/")
}
fn node(c: &Connection, id: u64) -> Result<Node> {
    let n=c.query_row("SELECT kind,mode,base,target,backing,size,nlink,parent,dirty,atime,mtime,ctime,origin FROM nodes WHERE id=?",[sql_id(id)],|r|{
        Ok((r.get::<_,i64>(0)?,r.get::<_,u32>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Vec<u8>>(3)?,r.get::<_,Option<String>>(4)?,read_id(r,5)?,read_id(r,6)?,read_id(r,7)?,r.get::<_,bool>(8)?,r.get::<_,i64>(9)?,r.get::<_,i64>(10)?,r.get::<_,i64>(11)?,r.get::<_,Option<String>>(12)?))
    }).optional()?.ok_or_else(||error(libc::ENOENT))?;
    Ok(Node {
        attr: VfsAttr {
            ino: id,
            kind: NodeKind::from_code(n.0)?,
            mode: n.1,
            size: n.5,
            nlink: n.6,
            parent: n.7,
            atime_ns: n.9,
            mtime_ns: n.10,
            ctime_ns: n.11,
        },
        base: n.2.map(|s| serde_json::from_str(&s)).transpose()?,
        target: n.3,
        backing: n.4,
        dirty: n.8,
        origin: n.12,
    })
}
fn require_dir(c: &Connection, id: u64) -> Result<Node> {
    let n = node(c, id)?;
    if n.attr.kind != NodeKind::Directory {
        return Err(error(libc::ENOTDIR));
    }
    Ok(n)
}
fn mutable(c: &Connection) -> Result<()> {
    if c.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key IN ('sealed','destroyed') AND value='true')",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(error(libc::EROFS));
    }
    Ok(())
}

/// Side-effect-free semantic validation for checkpoint-v3/build consumers.
/// Reading a snapshot is not itself a retention lease; caller must retain it.
/// Bounds:64MiB topology,100k groups,1M paths,128MiB cached directory metadata.
pub fn validate_snapshot(store: &LocalStore, snapshot: &VfsCheckpoint) -> Result<HardlinkTopology> {
    ensure!(
        snapshot.format_version == 1
            && snapshot.directory.kind == ObjectKind::Directory
            && snapshot.hardlinks.kind == ObjectKind::Blob,
        "unsupported VFS snapshot"
    );
    snapshot.directory.validate()?;
    snapshot.hardlinks.validate()?;
    ensure!(
        snapshot.directory.size_bytes <= 128 * 1024 * 1024
            && snapshot.hardlinks.size_bytes <= 64 * 1024 * 1024,
        "snapshot metadata exceeds consumer bound"
    );
    crate::tree::root_info(store, &snapshot.directory)?;
    let bytes = store.read_object(&snapshot.hardlinks)?;
    let topology: HardlinkTopology = serde_json::from_slice(&bytes)?;
    ensure!(
        topology.format_version == 1
            && topology.groups.len() <= 100_000
            && canonical_json(&topology)? == bytes,
        "invalid/noncanonical topology"
    );
    let mut paths = BTreeSet::new();
    let mut previous_group = None;
    for group in &topology.groups {
        ensure!(
            (2..=100_000).contains(&group.paths_hex.len()),
            "hardlink group size exceeds bound"
        );
        let mut previous = None;
        let mut expected = None;
        for raw in &group.paths_hex {
            let path = decode_path(raw)?;
            ensure!(
                previous.as_ref().is_none_or(|p| p < &path)
                    && paths.insert(path.clone())
                    && paths.len() <= 1_000_000,
                "unsorted/duplicate/excessive topology paths"
            );
            if previous.is_none() {
                ensure!(
                    previous_group.as_ref().is_none_or(|p| p < &path),
                    "unsorted topology groups"
                );
                previous_group = Some(path.clone());
            }
            previous = Some(path.clone());
            let mut directory = snapshot.directory.clone();
            let mut file = None;
            for (index, name) in path.iter().enumerate() {
                let entry = crate::tree::lookup(store, &directory, name)?
                    .context("topology path absent")?;
                match entry.node {
                    crate::directory::EntryNode::Directory { object } if index + 1 < path.len() => {
                        directory = object
                    }
                    crate::directory::EntryNode::File {
                        object,
                        logical_size,
                        mode,
                    } if index + 1 == path.len() => {
                        ensure!(
                            crate::content::file_info(store, &object)?.logical_size == logical_size,
                            "hardlink logical length differs from content"
                        );
                        file = Some((object, logical_size, mode));
                    }
                    _ => bail!("hardlink path must traverse directories and end at a regular file"),
                }
            }
            let file = file.context("hardlink path lacks file")?;
            if let Some(expected) = &expected {
                ensure!(expected == &file, "hardlink group content/mode differ");
            } else {
                expected = Some(file);
            }
        }
    }
    Ok(topology)
}

impl VfsManager {
    pub fn open(root: impl AsRef<Path>, store: LocalStore) -> Result<Self> {
        let root = root.as_ref();
        ensure!(root.is_absolute(), "VFS manager root must be absolute");
        if !root.exists() {
            fs::create_dir_all(root.parent().context("manager parent")?)?;
            secure_dir(root)?;
        }
        let marker = root.join("vfs-manager.json");
        let expected = serde_json::json!({"format_version":1,"store_root":store.root()});
        if marker.exists() {
            ensure!(
                serde_json::from_slice::<serde_json::Value>(&fs::read(&marker)?)? == expected,
                "VFS manager marker/store mismatch"
            );
        } else {
            ensure!(
                fs::read_dir(root)?.next().is_none(),
                "refusing nonempty non-VFS manager root"
            );
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&marker)?;
            f.write_all(&serde_json::to_vec(&expected)?)?;
            f.sync_all()?;
            File::open(root)?.sync_all()?;
        }
        secure_dir(root)?;
        for n in ["views", "mounts", "requests"] {
            secure_dir(&root.join(n))?;
        }
        let index = connection(&root.join("directories.sqlite"))?;
        index.execute_batch("CREATE TABLE IF NOT EXISTS directory_objects(digest TEXT PRIMARY KEY,reference TEXT NOT NULL,mode INTEGER NOT NULL,subdirectories INTEGER NOT NULL);CREATE TABLE IF NOT EXISTS directory_entries(digest TEXT NOT NULL,name BLOB NOT NULL,kind INTEGER NOT NULL,mode INTEGER NOT NULL,reference TEXT,target BLOB NOT NULL,PRIMARY KEY(digest,name)) WITHOUT ROWID;CREATE TABLE IF NOT EXISTS prepared(digest TEXT PRIMARY KEY,reference TEXT NOT NULL);CREATE TABLE IF NOT EXISTS topologies(digest TEXT NOT NULL,reference TEXT NOT NULL,directory TEXT NOT NULL,PRIMARY KEY(digest,directory));CREATE TABLE IF NOT EXISTS topology_paths(digest TEXT NOT NULL,path TEXT NOT NULL,group_id TEXT NOT NULL,nlink INTEGER NOT NULL,PRIMARY KEY(digest,path)) WITHOUT ROWID;CREATE INDEX IF NOT EXISTS topology_by_group ON topology_paths(digest,group_id);")?;
        index.execute_batch("CREATE TABLE IF NOT EXISTS prepared_paged(digest TEXT PRIMARY KEY,reference TEXT NOT NULL,mode INTEGER NOT NULL,entries INTEGER NOT NULL,subdirectories INTEGER NOT NULL);")?;
        File::open(root)?.sync_all()?;
        Ok(Self(Arc::new(Manager {
            root: root.canonicalize()?,
            store,
            index: Mutex::new(index),
            verified: Mutex::new((HashMap::new(), VecDeque::new())),
            remote: None,
        })))
    }
    pub fn root(&self) -> &Path {
        &self.0.root
    }
    pub fn store(&self) -> &LocalStore {
        &self.0.store
    }
    /// Full preparation/indexing is intentionally outside the fork fast path.
    pub fn prepare(&self, root: &ObjectRef) -> Result<()> {
        if self.0.remote.is_some() {
            return Err(error(libc::EOPNOTSUPP))
                .context("remote managers cannot prepare complete local roots");
        }
        root.validate()?;
        ensure!(
            root.kind == ObjectKind::Directory,
            "VFS root must be a directory"
        );
        let mut index = self.0.index.lock().map_err(|_| error(libc::EIO))?;
        if index
            .query_row(
                "SELECT reference FROM prepared WHERE digest=?",
                [&root.digest],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .as_deref()
            == Some(&json_ref(root)?)
        {
            self.store()
                .pin_existing(&format!("vfs:prepared:{}", root.digest), root)?;
            return Ok(());
        }
        let pin = format!("vfs:prepared:{}", root.digest);
        if self.store().retention()?.pins.values().any(|r| r == root) {
            self.store().pin_existing(&pin, root)?;
        } else {
            self.store().pin(&pin, root)?;
        }
        let tx = index.transaction()?;
        let mut stack = vec![(root.clone(), 0)];
        let mut seen = BTreeSet::new();
        while let Some((reference, depth)) = stack.pop() {
            ensure!(depth <= MAX_DEPTH, "directory depth limit");
            if !seen.insert(reference.digest.clone()) {
                continue;
            }
            let d = self.store().read_directory(&reference)?;
            tx.execute(
                "INSERT OR IGNORE INTO directory_objects VALUES(?,?,?,?)",
                params![
                    reference.digest,
                    json_ref(&reference)?,
                    d.mode,
                    d.entries
                        .iter()
                        .filter(|e| matches!(e.node, EntryNode::Directory { .. }))
                        .count() as i64
                ],
            )?;
            for e in d.entries {
                let name = e.name()?;
                let (kind, mode, child, target) = match e.node {
                    EntryNode::File { object, mode } => (1, mode, Some(object), Vec::new()),
                    EntryNode::Directory { object } => {
                        let mode = self.store().read_directory(&object)?.mode;
                        stack.push((object.clone(), depth + 1));
                        (2, mode, Some(object), Vec::new())
                    }
                    EntryNode::Symlink { target_hex } => (3, 0o777, None, hex::decode(target_hex)?),
                };
                tx.execute(
                    "INSERT OR IGNORE INTO directory_entries VALUES(?,?,?,?,?,?)",
                    params![
                        reference.digest,
                        name,
                        kind,
                        mode,
                        child.as_ref().map(json_ref).transpose()?,
                        target
                    ],
                )?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO prepared VALUES(?,?)",
            params![root.digest, json_ref(root)?],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn fork(&self, id: &str, root: &ObjectRef, owner: OwnerEpoch) -> Result<VfsView> {
        self.fork_inner(id, root, None, owner, false)
    }
    pub fn prepare_snapshot(&self, snapshot: &VfsCheckpoint) -> Result<()> {
        if self.0.remote.is_some() {
            return Err(error(libc::EOPNOTSUPP))
                .context("remote managers require explicit partial-root retention");
        }
        let topology = validate_snapshot(self.store(), snapshot)?;
        self.prepare(&snapshot.directory)?;
        let mut index = self.0.index.lock().map_err(|_| error(libc::EIO))?;
        let tx = index.transaction()?;
        for group in &topology.groups {
            let first = decode_path(&group.paths_hex[0])?;
            let group_id = encode_path(&first);
            for raw in &group.paths_hex {
                let key = encode_path(&decode_path(raw)?);
                tx.execute(
                    "INSERT OR REPLACE INTO topology_paths VALUES(?,?,?,?)",
                    params![
                        snapshot.hardlinks.digest,
                        key,
                        group_id,
                        group.paths_hex.len() as i64
                    ],
                )?;
            }
        }
        self.store().pin(
            &format!("vfs:topology:{}", snapshot.hardlinks.digest),
            &snapshot.hardlinks,
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO topologies VALUES(?,?,?)",
            params![
                snapshot.hardlinks.digest,
                json_ref(&snapshot.hardlinks)?,
                json_ref(&snapshot.directory)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn fork_snapshot(
        &self,
        id: &str,
        snapshot: &VfsCheckpoint,
        owner: OwnerEpoch,
    ) -> Result<VfsView> {
        let index = self.0.index.lock().map_err(|_| error(libc::EIO))?;
        let pair: Option<(String, String)> = index
            .query_row(
                "SELECT reference,directory FROM topologies WHERE digest=? AND directory=?",
                params![snapshot.hardlinks.digest, json_ref(&snapshot.directory)?],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        ensure!(
            pair == Some((
                json_ref(&snapshot.hardlinks)?,
                json_ref(&snapshot.directory)?
            )),
            "snapshot topology must be prepared"
        );
        drop(index);
        self.fork_inner(
            id,
            &snapshot.directory,
            Some(&snapshot.hardlinks),
            owner,
            false,
        )
    }
    /// Caller must stop all source writers, including descendants. This API
    /// performs a normal import; it does not claim to snapshot live files.
    pub fn prepare_from_quiesced_path(
        &self,
        path: &Path,
        policy: &ImportPolicy,
    ) -> Result<VfsCheckpoint> {
        if self.0.remote.is_some() {
            return Err(error(libc::EOPNOTSUPP))
                .context("remote managers cannot silently import an inherited SDK");
        }
        let path = path.canonicalize()?;
        for owned in [self.root(), self.store().root()] {
            if let Ok(relative) = owned.strip_prefix(&path) {
                let components = relative
                    .components()
                    .map(|c| c.as_os_str().as_bytes().to_vec())
                    .collect::<Vec<_>>();
                ensure!(
                    policy
                        .excluded_paths
                        .iter()
                        .any(|p| components.starts_with(p)),
                    "input contains live VFS/CAS metadata without an explicit exclusion"
                );
            }
        }
        let staging = self.store().begin_staging(Duration::from_secs(3600))?;
        let result = (|| {
            let directory = self.store().import_tree_with_policy(
                &path,
                crate::protocol::CheckpointConsistency::Quiesced,
                policy,
            )?;
            let mut groups = BTreeMap::<(u64, u64), Vec<Vec<Vec<u8>>>>::new();
            fn walk(
                path: &Path,
                relative: &mut Vec<Vec<u8>>,
                policy: &ImportPolicy,
                groups: &mut BTreeMap<(u64, u64), Vec<Vec<Vec<u8>>>>,
            ) -> Result<()> {
                ensure!(relative.len() <= MAX_DEPTH, "import topology depth limit");
                for item in fs::read_dir(path)? {
                    let item = item?;
                    relative.push(item.file_name().as_bytes().to_vec());
                    if !policy
                        .excluded_paths
                        .iter()
                        .any(|p| relative.starts_with(p))
                    {
                        let meta = fs::symlink_metadata(item.path())?;
                        if meta.is_dir() {
                            walk(&item.path(), relative, policy, groups)?
                        } else if meta.is_file() && meta.nlink() > 1 {
                            groups
                                .entry((meta.dev(), meta.ino()))
                                .or_default()
                                .push(relative.clone());
                        }
                    }
                    relative.pop();
                }
                Ok(())
            }
            walk(&path, &mut Vec::new(), policy, &mut groups)?;
            let mut paths = groups
                .into_values()
                .filter(|p| p.len() > 1)
                .collect::<Vec<_>>();
            for p in &mut paths {
                p.sort()
            }
            paths.sort();
            let topology = HardlinkTopology {
                format_version: 1,
                groups: paths
                    .into_iter()
                    .map(|paths| HardlinkGroup {
                        paths_hex: paths
                            .into_iter()
                            .map(|p| p.iter().map(hex::encode).collect())
                            .collect(),
                    })
                    .collect(),
            };
            let hardlinks = self.store().put_blob(&canonical_json(&topology)?)?;
            let snapshot = VfsCheckpoint {
                format_version: 1,
                directory,
                hardlinks,
            };
            self.prepare_snapshot(&snapshot)?;
            Ok(snapshot)
        })();
        let cleanup = self.store().abandon_staging(&staging.id);
        match result {
            Ok(v) => {
                cleanup?;
                Ok(v)
            }
            Err(e) => Err(e),
        }
    }
    fn fork_inner(
        &self,
        id: &str,
        root: &ObjectRef,
        topology: Option<&ObjectRef>,
        owner: OwnerEpoch,
        v2: bool,
    ) -> Result<VfsView> {
        self.fork_inner_with_namespace(id, root, topology, owner, v2, None)
    }
    fn fork_inner_with_namespace(
        &self,
        id: &str,
        root: &ObjectRef,
        topology: Option<&ObjectRef>,
        owner: OwnerEpoch,
        v2: bool,
        observation_namespace: Option<&str>,
    ) -> Result<VfsView> {
        ensure!(Uuid::parse_str(id).is_ok(), "view id must be a UUID");
        if let Some(namespace) = observation_namespace {
            ensure!(
                v2 && Uuid::parse_str(namespace)?.to_string() == namespace,
                "reserved observation namespace requires canonical paged profile"
            );
        }
        owner.validate()?;
        if let Some(remote) = &self.0.remote {
            remote.guard()?;
            ensure!(
                v2 && topology.is_none()
                    && id == remote.view_id()
                    && owner == *remote.owner()
                    && root == remote.base(),
                "remote view differs from current session binding"
            );
        }
        let fork_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root().join("fork.lock"))?;
        fork_lock.lock_exclusive()?;
        let reservation = self.root().join("requests").join(format!("{id}.json"));
        let view_format = if self.0.remote.is_some() {
            3
        } else if v2 {
            2
        } else {
            1
        };
        let mut request = serde_json::json!({"format_version":view_format,"view_id":id,"root":root,"topology":topology,"owner":owner});
        if let Some(remote) = &self.0.remote {
            request["remote_binding"] = serde_json::from_str(&remote.serialized_binding()?)?;
        }
        if let Some(namespace) = observation_namespace {
            request["observation_namespace"] = serde_json::Value::String(namespace.into());
        }
        let request = canonical_json(&request)?;
        if reservation.exists() {
            let saved = if observation_namespace.is_some() {
                let mut input = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                    .open(&reservation)?;
                let meta = input.metadata()?;
                ensure!(
                    meta.is_file() && meta.uid() == unsafe { libc::geteuid() }
                        && meta.mode() & 0o077 == 0 && meta.nlink() == 1,
                    "unsafe reserved remote fork request"
                );
                if meta.len() > 64 * 1024 {
                    return Err(error(libc::EFBIG)).context("remote fork request exceeds 64 KiB");
                }
                let mut bytes = Vec::new();
                Read::by_ref(&mut input).take(64 * 1024 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 64 * 1024 {
                    return Err(error(libc::EFBIG)).context("remote fork request grew beyond 64 KiB");
                }
                bytes
            } else {
                fs::read(&reservation)?
            };
            ensure!(
                saved == request,
                "view id already reserved for different fork inputs"
            );
        } else {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&reservation)?;
            f.write_all(&request)?;
            f.sync_all()?;
            File::open(reservation.parent().unwrap())?.sync_all()?;
        }
        let index = self.0.index.lock().map_err(|_| error(libc::EIO))?;
        let prepared: Option<(String, u32, u64, u64)> = if v2 {
            index.query_row("SELECT reference,mode,subdirectories,entries FROM prepared_paged WHERE digest=?",[&root.digest],|r|Ok((r.get(0)?,r.get(1)?,read_id(r,2)?,read_id(r,3)?))).optional()?
        } else {
            index.query_row("SELECT p.reference,d.mode,d.subdirectories FROM prepared p JOIN directory_objects d ON p.digest=d.digest WHERE p.digest=?",[&root.digest],|r|Ok((r.get(0)?,r.get(1)?,read_id(r,2)?,0))).optional()?
        };
        let (reference, mode, subdirectories, entries) =
            prepared.context("root must be prepared for this data plane before fork")?;
        ensure!(reference == json_ref(root)?, "prepared root mismatch");
        drop(index);
        let final_path = self.root().join("views").join(id);
        if final_path.exists() {
            let view = self.open_view(id, &owner)?;
            ensure!(
                view.descriptor()?.base == *root,
                "idempotent fork root differs"
            );
            return Ok(view);
        }
        if self.0.remote.is_none() {
            self.store()
                .pin_existing(&format!("vfs:view:{id}:base"), root)?;
            if let Some(t) = topology {
                self.store()
                    .pin_existing(&format!("vfs:view:{id}:topology"), t)?;
            }
        }
        // Publish a complete database by directory rename. Crash leftovers stay
        // hidden and retained; retries never open a half-created view database.
        let path = self
            .root()
            .join("views")
            .join(format!(".pending-{}", Uuid::new_v4()));
        secure_dir(&path)?;
        secure_dir(&path.join("files"))?;
        let mut db = connection(&path.join("namespace.sqlite"))?;
        db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE nodes(id INTEGER PRIMARY KEY AUTOINCREMENT,kind INTEGER NOT NULL,mode INTEGER NOT NULL,base TEXT,target BLOB NOT NULL,backing TEXT,size INTEGER NOT NULL,nlink INTEGER NOT NULL,parent INTEGER NOT NULL,dirty INTEGER NOT NULL,atime INTEGER NOT NULL,mtime INTEGER NOT NULL,ctime INTEGER NOT NULL,origin TEXT);CREATE INDEX nodes_origin ON nodes(origin);CREATE TABLE dirents(parent INTEGER NOT NULL,name BLOB NOT NULL,child INTEGER,PRIMARY KEY(parent,name)) WITHOUT ROWID;CREATE INDEX dirents_by_child ON dirents(child);CREATE TABLE inherited_groups(group_id TEXT PRIMARY KEY,inode INTEGER NOT NULL);")?;
        if v2 {
            content::initialize(&db)?;
            namespace::initialize(&db, entries)?;
            if let Some(namespace) = observation_namespace {
                observation::initialize_reserved(&db, namespace)?;
            } else {
                observation::initialize(&db)?;
            }
        }
        if self.0.remote.is_some() {
            readdir::initialize(&db)?;
        }
        let tx = db.transaction()?;
        let time = now();
        for (k, v) in [
            ("format_version", view_format.to_string()),
            ("view_id", id.to_string()),
            ("base", reference.clone()),
            ("owner", serde_json::to_string(&owner)?),
        ] {
            tx.execute("INSERT INTO meta VALUES(?,?)", params![k, v])?;
        }
        if let Some(remote) = &self.0.remote {
            tx.execute(
                "INSERT INTO meta VALUES('remote_binding',?)",
                [remote.serialized_binding()?],
            )?;
        }
        if let Some(t) = topology {
            tx.execute("INSERT INTO meta VALUES('topology',?)", [json_ref(t)?])?;
        }
        tx.execute(
            "INSERT INTO nodes VALUES(1,2,?,?,?,NULL,0,?,1,0,?,?,?,'')",
            params![
                mode,
                reference,
                Vec::<u8>::new(),
                sql_id(
                    subdirectories
                        .checked_add(2)
                        .filter(|v| *v <= i64::MAX as u64)
                        .context("directory link count overflow")?
                ),
                time,
                time,
                time
            ],
        )?;
        tx.commit()?;
        drop(db);
        File::open(&path)?.sync_all()?;
        fs::rename(&path, &final_path)?;
        File::open(final_path.parent().unwrap())?.sync_all()?;
        self.open_view(id, &owner)
    }
    pub fn open_view(&self, id: &str, owner: &OwnerEpoch) -> Result<VfsView> {
        ensure!(Uuid::parse_str(id).is_ok(), "invalid view id");
        let path = self.root().join("views").join(id);
        ensure!(
            fs::symlink_metadata(&path)?.is_dir()
                && !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "invalid view path"
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.join("owner.lock"))?;
        lock.try_lock_exclusive()
            .context("view already owned by another filesystem process")?;
        let db = connection(&path.join("namespace.sqlite"))?;
        let stored_remote: Option<String> = db
            .query_row(
                "SELECT value FROM meta WHERE key='remote_binding'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match (&self.0.remote, stored_remote) {
            (Some(remote), Some(stored)) => {
                remote.guard()?;
                ensure!(
                    stored == remote.serialized_binding()?
                        && id == remote.view_id()
                        && owner == remote.owner(),
                    "persisted remote view binding differs"
                );
            }
            (None, None) => {}
            _ => {
                return Err(error(libc::EOPNOTSUPP)).context(
                    "remote views require their exact fresh session; local mode cannot reopen them",
                )
            }
        }
        let stored: String =
            db.query_row("SELECT value FROM meta WHERE key='owner'", [], |r| r.get(0))?;
        ensure!(
            serde_json::from_str::<OwnerEpoch>(&stored)? == *owner,
            "stale VFS owner epoch"
        );
        let version = db.query_row(
            "SELECT value FROM meta WHERE key='format_version'",
            [],
            |r| r.get::<_, String>(0),
        )?;
        ensure!(
            version == "1" || version == "2" || version == "3" || version == "4",
            "unsupported VFS version"
        );
        ensure!(
            (version == "3" || version == "4") == self.0.remote.is_some(),
            "remote pager requires namespace3/4; legacy remote namespaces require migration"
        );
        let v2 = version == "2" || version == "3" || version == "4";
        if version == "4" {
            capture::validate_schema(&db, self.0.remote.as_deref())?;
        }
        ensure!(
            db.query_row("SELECT value FROM meta WHERE key='view_id'", [], |r| r
                .get::<_, String>(
                0
            ))? == id,
            "view id mismatch"
        );
        if let Some(request) = self.0.remote.as_ref().and_then(|remote| remote.launch_request()) {
            let namespace: String = db.query_row(
                "SELECT namespace FROM observation_state WHERE singleton=1", [], |row| row.get(0),
            )?;
            ensure!(
                namespace == request.observation.namespace,
                "persisted remote observation namespace differs from reserved launch"
            );
        }
        db.execute(
            "ATTACH DATABASE ? AS inherited",
            [self
                .root()
                .join("directories.sqlite")
                .to_string_lossy()
                .as_ref()],
        )?;
        let pager = self
            .0
            .remote
            .as_ref()
            .map(|remote| {
                readdir::Pager::open(
                    &path.join("readdir.sqlite"),
                    remote.readdir_limits().clone(),
                )
                .map(Mutex::new)
            })
            .transpose()?;
        let view = VfsView(Arc::new(View {
            manager: self.clone(),
            id: id.to_string(),
            owner: owner.clone(),
            path,
            db: Mutex::new(db),
            _lock: lock,
            mounted: AtomicBool::new(false),
            next_handle: AtomicU64::new(1),
            handles: Mutex::new(HashMap::new()),
            pager,
            v2,
        }));
        // Validate every recorded private backing before serving: never replace a
        // missing only copy with inherited bytes. Unreferenced files are retained.
        {
            let c = view.db()?;
            let mut q = c.prepare("SELECT backing FROM nodes WHERE backing IS NOT NULL")?;
            let names = q.query_map([], |r| r.get::<_, String>(0))?;
            for n in names {
                view.private_file(&n?, false)?;
            }
            if v2 {
                content::validate_logs(&view, &c)?;
            }
        }
        Ok(view)
    }
}

impl VfsView {
    fn db(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        if let Some(remote) = &self.0.manager.0.remote {
            remote.guard()?;
        }
        self.0.db.lock().map_err(|_| error(libc::EIO))
    }
    pub(crate) fn require_local_checkpoint(&self) -> Result<()> {
        if self.manager().0.remote.is_some() {
            return Err(error(libc::EOPNOTSUPP)).context("partial remote views require explicit partial capture/publication; complete checkpoint and observation remain unsupported");
        }
        Ok(())
    }
    pub fn id(&self) -> &str {
        &self.0.id
    }
    pub fn manager(&self) -> &VfsManager {
        &self.0.manager
    }
    pub fn descriptor(&self) -> Result<VfsDescriptor> {
        let c = self.db()?;
        let get = |key: &str| -> Result<String> {
            Ok(c.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))?)
        };
        Ok(VfsDescriptor {
            format_version: if self.manager().0.remote.is_some() {
                3
            } else if self.0.v2 {
                2
            } else {
                1
            },
            view_id: self.0.id.clone(),
            owner: self.0.owner.clone(),
            base: serde_json::from_str(&get("base")?)?,
            latest: c
                .query_row("SELECT value FROM meta WHERE key='latest'", [], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
                .map(|s| serde_json::from_str(&s))
                .transpose()?,
            latest_snapshot: c
                .query_row(
                    "SELECT value FROM meta WHERE key='latest_snapshot'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .map(|s| serde_json::from_str(&s))
                .transpose()?,
            inode_count: c.query_row("SELECT count(*) FROM nodes", [], |r| read_id(r, 0))?,
            private_dirent_count: c
                .query_row("SELECT count(*) FROM dirents", [], |r| read_id(r, 0))?,
            partial_capture_format: (get("format_version")? == "4").then_some(1),
            data_plane: self.0.v2.then(|| {
                if self.manager().0.remote.is_some() {
                    "remote_paged_ranges_v1".to_string()
                } else {
                    "paged_ranges_v2".to_string()
                }
            }),
        })
    }
    fn private_file(&self, name: &str, write: bool) -> Result<File> {
        ensure!(Uuid::parse_str(name).is_ok(), "invalid private backing id");
        let p = self.0.path.join("files").join(name);
        let f = OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(p)?;
        ensure!(f.metadata()?.is_file(), "private backing is not a file");
        Ok(f)
    }
    fn inherited_file(&self, r: &ObjectRef) -> Result<Arc<File>> {
        if self.manager().0.remote.is_some() {
            return Err(error(libc::EOPNOTSUPP)).context(
                "remote inherited objects require proof-bound ranges, never local CAS FD fallback",
            );
        }
        ensure!(
            r.kind == ObjectKind::Blob,
            "immutable FD resolver requires a raw Blob"
        );
        r.validate()?;
        let mut cache = self
            .0
            .manager
            .0
            .verified
            .lock()
            .map_err(|_| error(libc::EIO))?;
        if let Some(f) = cache.0.get(r) {
            return Ok(f.clone());
        }
        // A borrowed FD remains in the cache until its readers release it. Do
        // not evict busy Arcs and silently exceed the descriptor limit across
        // views. Admission fails explicitly if every bounded slot is in use.
        if cache.0.len() >= MAX_IMMUTABLE_FDS {
            for _ in 0..cache.1.len() {
                let key = cache
                    .1
                    .pop_front()
                    .context("immutable FD cache queue differs")?;
                if cache
                    .0
                    .get(&key)
                    .is_some_and(|file| Arc::strong_count(file) == 1)
                {
                    cache.0.remove(&key);
                    break;
                }
                cache.1.push_back(key);
            }
            if cache.0.len() >= MAX_IMMUTABLE_FDS {
                return Err(error(libc::EAGAIN));
            }
        }
        let mut f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(self.manager().store().object_path(r)?)?;
        let meta = f.metadata()?;
        ensure!(
            meta.is_file() && meta.len() == r.size_bytes,
            "CAS file size/type differs"
        );
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut read = 0u64;
        loop {
            let n = f.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            read = read.checked_add(n as u64).context("CAS size overflow")?;
            ensure!(read <= r.size_bytes, "CAS object grew during verification");
            hash.update(&buffer[..n]);
        }
        ensure!(
            read == r.size_bytes && hex::encode(hash.finalize()) == r.digest,
            "CAS checksum mismatch"
        );
        let f = Arc::new(f);
        cache.0.insert(r.clone(), f.clone());
        cache.1.push_back(r.clone());
        Ok(f)
    }
    fn lookup_tx(&self, c: &Connection, parent: u64, name: &[u8]) -> Result<Node> {
        self.lookup_tx_metered(c, parent, name, None)
    }
    fn lookup_tx_metered(
        &self,
        c: &Connection,
        parent: u64,
        name: &[u8],
        meter: Option<&observation::WorkMeter<'_>>,
    ) -> Result<Node> {
        if name == b"." {
            return require_dir(c, parent);
        }
        if name == b".." {
            let p = require_dir(c, parent)?.attr.parent;
            return node(c, p);
        }
        name_ok(name)?;
        let p = require_dir(c, parent)?;
        if let Some(child) = c
            .query_row(
                "SELECT child FROM dirents WHERE parent=? AND name=?",
                params![sql_id(parent), name],
                |r| r.get::<_, Option<i64>>(0).map(|v| v.map(|v| v as u64)),
            )
            .optional()?
        {
            return match child {
                Some(id) => node(c, id),
                None => Err(error(libc::ENOENT)),
            };
        }
        self.materialize_inherited_tx(c, &p, parent, name, true, meter)
    }
    fn materialize_inherited_tx(
        &self,
        c: &Connection,
        p: &Node,
        parent: u64,
        name: &[u8],
        bind: bool,
        meter: Option<&observation::WorkMeter<'_>>,
    ) -> Result<Node> {
        let base = p.base.clone().ok_or_else(|| error(libc::ENOENT))?;
        let origin = p.origin.as_ref().map(|p| {
            if p.is_empty() {
                hex::encode(name)
            } else {
                format!("{p}/{}", hex::encode(name))
            }
        });
        let mut logical_size = None;
        let mut directory_counts = None;
        let e = if self.0.v2 {
            let entry = namespace::view_lookup(self, &p, &base, name, meter)?
                .ok_or_else(|| error(libc::ENOENT))?;
            match entry.node {
                crate::directory::EntryNode::File {
                    object,
                    logical_size: size,
                    mode,
                } => {
                    logical_size = Some(size);
                    (1, mode, Some(json_ref(&object)?), Vec::new())
                }
                crate::directory::EntryNode::Directory { object } => {
                    let (mode, entries, subdirectories) =
                        namespace::view_header(self, origin.as_deref(), &object, meter)?;
                    directory_counts = Some((entries, subdirectories));
                    (2, mode, Some(json_ref(&object)?), Vec::new())
                }
                crate::directory::EntryNode::Symlink { target_hex } => {
                    (3, 0o777, None, hex::decode(target_hex)?)
                }
            }
        } else {
            c.query_row("SELECT kind,mode,reference,target FROM inherited.directory_entries WHERE digest=? AND name=?",params![base.digest,name],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,u32>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Vec<u8>>(3)?))).optional()?.ok_or_else(||error(libc::ENOENT))?
        };
        let reference =
            e.2.as_ref()
                .map(|s| serde_json::from_str::<ObjectRef>(s))
                .transpose()?;
        let size = logical_size.unwrap_or_else(|| {
            reference
                .as_ref()
                .filter(|r| r.kind == ObjectKind::Blob)
                .map(|r| r.size_bytes)
                .unwrap_or(e.3.len() as u64)
        });
        let t = now();
        let topology = c
            .query_row("SELECT value FROM meta WHERE key='topology'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(|s| serde_json::from_str::<ObjectRef>(&s))
            .transpose()?;
        let group = match (topology, origin.as_ref()) {
            (Some(t), Some(p)) => c
                .query_row(
                    "SELECT group_id,nlink FROM inherited.topology_paths WHERE digest=? AND path=?",
                    params![t.digest, p],
                    |r| Ok((r.get::<_, String>(0)?, read_id(r, 1)?)),
                )
                .optional()?,
            _ => None,
        };
        if let Some((g, _)) = &group {
            if let Some(id) = c
                .query_row(
                    "SELECT inode FROM inherited_groups WHERE group_id=?",
                    [g],
                    |r| read_id(r, 0),
                )
                .optional()?
            {
                c.execute(
                    "INSERT INTO dirents VALUES(?,?,?)",
                    params![sql_id(parent), name, sql_id(id)],
                )?;
                return node(c, id);
            }
        }
        let nlink = if e.0 == 2 {
            let subdirectories = if let Some((_, subdirectories)) = directory_counts {
                subdirectories
            } else {
                c.query_row(
                    "SELECT subdirectories FROM inherited.directory_objects WHERE digest=?",
                    [&reference.as_ref().context("directory ref missing")?.digest],
                    |r| read_id(r, 0),
                )?
            };
            subdirectories
                .checked_add(2)
                .filter(|v| *v <= i64::MAX as u64)
                .context("directory link count overflow")?
        } else {
            group.as_ref().map(|(_, n)| *n).unwrap_or(1)
        };
        let reserved = if let Some(remote) = &self.manager().0.remote {
            remote.check_origin(origin.as_deref().context("remote inode origin missing")?)?;
            Some(readdir::reserve(
                c,
                origin.as_deref().context("remote inode origin missing")?,
                parent,
                name,
                NodeKind::from_code(e.0)?,
                remote.readdir_limits(),
            )?)
        } else {
            None
        };
        let id = if let Some(id) = reserved {
            if let Some(existing) = c
                .query_row("SELECT id FROM nodes WHERE id=?", [sql_id(id)], |r| {
                    read_id(r, 0)
                })
                .optional()?
            {
                if bind {
                    c.execute(
                        "INSERT INTO dirents VALUES(?,?,?)",
                        params![sql_id(parent), name, sql_id(existing)],
                    )?;
                }
                return node(c, existing);
            }
            c.execute("INSERT INTO nodes(id,kind,mode,base,target,backing,size,nlink,parent,dirty,atime,mtime,ctime,origin) VALUES(?,?,?,?,?,NULL,?,?,?,0,?,?,?,?)",params![sql_id(id),e.0,e.1,e.2,e.3,sql_id(size),sql_id(nlink),sql_id(parent),t,t,t,origin])?;
            id
        } else {
            c.execute("INSERT INTO nodes(kind,mode,base,target,backing,size,nlink,parent,dirty,atime,mtime,ctime,origin) VALUES(?,?,?,?,NULL,?,?,?,0,?,?,?,?)",params![e.0,e.1,e.2,e.3,sql_id(size),sql_id(nlink),sql_id(parent),t,t,t,origin])?;
            c.last_insert_rowid() as u64
        };
        if let Some((entries, _)) = directory_counts {
            ensure!(
                entries <= i64::MAX as u64,
                "directory count exceeds private namespace capacity"
            );
            c.execute(
                "INSERT INTO directory_sizes VALUES(?,?)",
                params![sql_id(id), sql_id(entries)],
            )?;
        }
        if bind {
            c.execute(
                "INSERT INTO dirents VALUES(?,?,?)",
                params![sql_id(parent), name, sql_id(id)],
            )?;
        }
        if let Some((g, _)) = group {
            c.execute(
                "INSERT INTO inherited_groups VALUES(?,?)",
                params![g, sql_id(id)],
            )?;
        }
        node(c, id)
    }
    pub fn lookup(&self, parent: u64, name: &[u8]) -> Result<VfsAttr> {
        let mut c = self.db()?;
        let tx = c.transaction()?;
        let n = self.lookup_tx(&tx, parent, name)?;
        tx.commit()?;
        self.actual_attr(n)
    }
    pub fn attr(&self, id: u64) -> Result<VfsAttr> {
        let mut c = self.db()?;
        let tx = c.transaction()?;
        let n = match node(&tx, id) {
            Ok(n) => n,
            Err(e) if errno(&e) == libc::ENOENT && self.manager().0.remote.is_some() => {
                let (parent, name, origin, kind) = readdir::reserved(&tx, id)?.ok_or(e)?;
                let p = require_dir(&tx, parent)?;
                let n = self.materialize_inherited_tx(&tx, &p, parent, &name, false, None)?;
                ensure!(
                    n.attr.ino == id && n.origin.as_ref() == Some(&origin) && n.attr.kind == kind,
                    "materialized reservation identity differs"
                );
                n
            }
            Err(e) => return Err(e),
        };
        tx.commit()?;
        self.actual_attr(n)
    }
    fn actual_attr(&self, mut n: Node) -> Result<VfsAttr> {
        if let Some(b) = n.backing.filter(|_| !self.0.v2) {
            n.attr.size = self.private_file(&b, false)?.metadata()?.len()
        }
        Ok(n.attr)
    }
    fn dirty(c: &Connection, start: u64) -> Result<()> {
        let mut id = start;
        for _ in 0..=MAX_DEPTH {
            let n = node(c, id)?;
            if id != 1 {
                namespace::bindings_changed(c, id)?;
            }
            c.execute(
                "UPDATE nodes SET dirty=1,mtime=?,ctime=? WHERE id=?",
                params![now(), now(), sql_id(id)],
            )?;
            if id == 1 {
                return Ok(());
            }
            id = n.attr.parent;
        }
        bail!("directory ancestor cycle/depth limit")
    }
    fn dirty_file(c: &Connection, id: u64) -> Result<()> {
        namespace::bindings_changed(c, id)?;
        c.execute(
            "UPDATE nodes SET dirty=1,mtime=?,ctime=? WHERE id=?",
            params![now(), now(), sql_id(id)],
        )?;
        let mut q = c.prepare("SELECT DISTINCT parent FROM dirents WHERE child=?")?;
        let parents = q
            .query_map([sql_id(id)], |r| read_id(r, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        for p in parents {
            Self::dirty(c, p)?;
        }
        Ok(())
    }
    pub fn create(
        &self,
        parent: u64,
        name: &[u8],
        kind: NodeKind,
        mode: u32,
        target: &[u8],
    ) -> Result<VfsAttr> {
        name_ok(name)?;
        ensure!(mode <= 0o7777, "invalid mode");
        if kind == NodeKind::Symlink {
            ensure!(
                !target.is_empty() && !target.contains(&0),
                "invalid link target"
            )
        }
        let mut c = self.db()?;
        let tx = c.transaction()?;
        mutable(&tx)?;
        require_dir(&tx, parent)?;
        match self.lookup_tx(&tx, parent, name) {
            Ok(_) => return Err(error(libc::EEXIST)),
            Err(e) if errno(&e) == libc::ENOENT => {}
            Err(e) => return Err(e),
        }
        let backing = if kind == NodeKind::File {
            Some(self.new_backing(None)?)
        } else {
            None
        };
        let t = now();
        tx.execute("INSERT INTO nodes(kind,mode,base,target,backing,size,nlink,parent,dirty,atime,mtime,ctime,origin) VALUES(?,?,NULL,?,?,?,?,?,1,?,?,?,NULL)",params![kind.code(),mode,target,backing,target.len() as i64,if kind==NodeKind::Directory{2}else{1},sql_id(parent),t,t,t])?;
        let id = tx.last_insert_rowid() as u64;
        if self.0.v2 && kind == NodeKind::Directory {
            tx.execute("INSERT INTO directory_sizes VALUES(?,0)", [sql_id(id)])?;
        }
        if kind == NodeKind::Directory {
            tx.execute(
                "UPDATE nodes SET nlink=nlink+1 WHERE id=?",
                [sql_id(parent)],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO dirents VALUES(?,?,?)",
            params![sql_id(parent), name, sql_id(id)],
        )?;
        namespace::changed(&tx, parent, name)?;
        namespace::count_delta(&tx, parent, 1)?;
        Self::dirty(&tx, parent)?;
        observation::mutation(&tx, "create", id, Some(parent), Some(name), None)?;
        let n = node(&tx, id)?;
        tx.commit()?;
        self.actual_attr(n)
    }
    fn new_backing(&self, base: Option<&ObjectRef>) -> Result<String> {
        if base.is_some() && self.manager().0.remote.is_some() {
            return Err(error(libc::EOPNOTSUPP))
                .context("remote copy-up must use private range blocks");
        }
        let id = Uuid::new_v4().to_string();
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.0.path.join("files").join(&id))?;
        if let Some(r) = base {
            let bytes = self.manager().store().read_object(r)?;
            f.write_all(&bytes)?;
        }
        f.sync_all()?;
        File::open(self.0.path.join("files"))?.sync_all()?;
        Ok(id)
    }
    fn writable(&self, c: &Connection, id: u64) -> Result<File> {
        mutable(c)?;
        let n = node(c, id)?;
        if n.attr.kind != NodeKind::File {
            return Err(error(libc::EISDIR));
        }
        let backing = match n.backing {
            Some(s) => s,
            None => {
                let s = self.new_backing(n.base.as_ref())?;
                c.execute(
                    "UPDATE nodes SET backing=? WHERE id=?",
                    params![s, sql_id(id)],
                )?;
                s
            }
        };
        self.private_file(&backing, true)
    }
    pub fn read(&self, id: u64, offset: u64, size: usize) -> Result<Vec<u8>> {
        ensure!(size <= MAX_IO, "read exceeds 16 MiB");
        if self.0.v2 {
            let c = self.db()?;
            let n = node(&c, id)?;
            if n.attr.kind != NodeKind::File {
                return Err(error(libc::EISDIR));
            }
            return content::read(self, &c, &n, offset, size);
        }
        let n = node(&*self.db()?, id)?;
        if n.attr.kind != NodeKind::File {
            return Err(error(libc::EISDIR));
        }
        let f = match n.backing {
            Some(s) => Arc::new(self.private_file(&s, false)?),
            None => self.inherited_file(n.base.as_ref().context("file without content")?)?,
        };
        let mut b = vec![0; size];
        let mut done = 0;
        while done < size {
            let read = f.read_at(
                &mut b[done..],
                offset.checked_add(done as u64).context("read overflow")?,
            )?;
            if read == 0 {
                break;
            }
            done += read;
        }
        b.truncate(done);
        Ok(b)
    }
    fn hydrate_links(&self, c: &Connection, id: u64) -> Result<()> {
        if self.0.v2 {
            return namespace::hydrate_links(self, c, id);
        }
        let group = c
            .query_row(
                "SELECT group_id FROM inherited_groups WHERE inode=?",
                [sql_id(id)],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        let Some(group) = group else { return Ok(()) };
        let raw: String = c.query_row("SELECT value FROM meta WHERE key='topology'", [], |r| {
            r.get(0)
        })?;
        let topology: ObjectRef = serde_json::from_str(&raw)?;
        let mut q =
            c.prepare("SELECT path FROM inherited.topology_paths WHERE digest=? AND group_id=?")?;
        let paths = q
            .query_map(params![topology.digest, group], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        for path in paths {
            let parts = path.split('/').collect::<Vec<_>>();
            let mut parent = 1;
            let mut origin = String::new();
            let mut reachable = true;
            for part in &parts[..parts.len() - 1] {
                if !origin.is_empty() {
                    origin.push('/')
                }
                origin.push_str(part);
                if let Some(found) = c
                    .query_row(
                        "SELECT id FROM nodes WHERE origin=? AND kind=2 AND nlink>0",
                        [&origin],
                        |r| read_id(r, 0),
                    )
                    .optional()?
                {
                    parent = found;
                } else {
                    match self.lookup_tx(c, parent, &hex::decode(part)?) {
                        Ok(n) if n.attr.kind == NodeKind::Directory => parent = n.attr.ino,
                        _ => {
                            reachable = false;
                            break;
                        }
                    }
                }
            }
            if reachable {
                let _ = self.lookup_tx(c, parent, &hex::decode(parts.last().unwrap())?);
            }
        }
        Ok(())
    }
    pub fn write(&self, id: u64, offset: u64, bytes: &[u8], sync: bool) -> Result<usize> {
        ensure!(bytes.len() <= MAX_IO, "write exceeds16MiB");
        ensure!(
            offset
                .checked_add(bytes.len() as u64)
                .is_some_and(|v| v <= i64::MAX as u64),
            "file offset overflow"
        );
        let mut c = self.db()?;
        let tx = c.transaction()?;
        if self.0.v2 {
            mutable(&tx)?;
            let n = node(&tx, id)?;
            if n.attr.kind != NodeKind::File {
                return Err(error(libc::EISDIR));
            }
            if bytes.is_empty() {
                ensure!(
                    n.attr.kind == NodeKind::File,
                    "zero write requires a regular file"
                );
                return Ok(0);
            }
            content::write(self, &tx, &n, offset, bytes)?;
        } else {
            let f = self.writable(&tx, id)?;
            f.write_all_at(bytes, offset)?;
            if sync {
                f.sync_all()?;
            }
        }
        self.hydrate_links(&tx, id)?;
        Self::dirty_file(&tx, id)?;
        observation::mutation(&tx, "write", id, None, None, None)?;
        tx.commit()?;
        Ok(bytes.len())
    }
    pub fn set_attr(
        &self,
        id: u64,
        mode: Option<u32>,
        size: Option<u64>,
        atime: Option<i64>,
        mtime: Option<i64>,
    ) -> Result<VfsAttr> {
        let mut c = self.db()?;
        let tx = c.transaction()?;
        mutable(&tx)?;
        let n = node(&tx, id)?;
        if let Some(mode) = mode {
            ensure!(mode <= 0o7777, "invalid mode");
            tx.execute(
                "UPDATE nodes SET mode=? WHERE id=?",
                params![mode, sql_id(id)],
            )?;
        }
        if let Some(size) = size {
            ensure!(size <= i64::MAX as u64, "file too large");
            if self.0.v2 {
                content::truncate(self, &tx, &n, size)?;
            } else {
                self.writable(&tx, id)?.set_len(size)?;
            }
        }
        if let Some(t) = atime {
            tx.execute(
                "UPDATE nodes SET atime=? WHERE id=?",
                params![t, sql_id(id)],
            )?;
        }
        if n.attr.kind == NodeKind::Directory {
            Self::dirty(&tx, id)?
        } else {
            self.hydrate_links(&tx, id)?;
            Self::dirty_file(&tx, id)?
        }
        let mtime = mtime.unwrap_or(if size.is_some() {
            now()
        } else {
            n.attr.mtime_ns
        });
        tx.execute(
            "UPDATE nodes SET mtime=? WHERE id=?",
            params![mtime, sql_id(id)],
        )?;
        observation::mutation(&tx, "set_attr", id, None, None, None)?;
        let n = node(&tx, id)?;
        tx.commit()?;
        self.actual_attr(n)
    }
    pub fn readlink(&self, id: u64) -> Result<Vec<u8>> {
        let n = node(&*self.db()?, id)?;
        if n.attr.kind != NodeKind::Symlink {
            return Err(error(libc::EINVAL));
        }
        Ok(n.target)
    }
    fn names(&self, c: &Connection, parent: u64) -> Result<Vec<Vec<u8>>> {
        if self.0.v2 {
            return namespace::names(self, c, parent);
        }
        let n = require_dir(c, parent)?;
        let digest = n.base.as_ref().map(|r| r.digest.as_str()).unwrap_or("");
        let mut q=c.prepare("SELECT name FROM inherited.directory_entries b WHERE digest=? AND NOT EXISTS(SELECT 1 FROM dirents p WHERE p.parent=? AND p.name=b.name) UNION SELECT name FROM dirents WHERE parent=? AND child IS NOT NULL ORDER BY name")?;
        let rows = q
            .query_map(params![digest, sql_id(parent), sql_id(parent)], |r| {
                r.get::<_, Vec<u8>>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    fn empty(&self, c: &Connection, parent: u64) -> Result<bool> {
        if self.0.v2 {
            return Ok(c.query_row(
                "SELECT entry_count=0 FROM directory_sizes WHERE inode=?",
                [sql_id(parent)],
                |r| r.get(0),
            )?);
        }
        let n = require_dir(c, parent)?;
        let digest = n.base.as_ref().map(|r| r.digest.as_str()).unwrap_or("");
        Ok(!c.query_row("SELECT EXISTS(SELECT 1 FROM dirents WHERE parent=? AND child IS NOT NULL UNION ALL SELECT 1 FROM inherited.directory_entries b WHERE digest=? AND NOT EXISTS(SELECT 1 FROM dirents p WHERE p.parent=? AND p.name=b.name) LIMIT 1)",params![sql_id(parent),digest,sql_id(parent)],|r|r.get::<_,bool>(0))?)
    }
    pub fn unlink(&self, parent: u64, name: &[u8], directory: bool) -> Result<()> {
        name_ok(name)?;
        let mut c = self.db()?;
        let tx = c.transaction()?;
        mutable(&tx)?;
        let n = self.lookup_tx(&tx, parent, name)?;
        if directory {
            if n.attr.kind != NodeKind::Directory {
                return Err(error(libc::ENOTDIR));
            }
            if !self.empty(&tx, n.attr.ino)? {
                return Err(error(libc::ENOTEMPTY));
            }
        } else if n.attr.kind == NodeKind::Directory {
            return Err(error(libc::EISDIR));
        }
        tx.execute(
            "INSERT OR REPLACE INTO dirents VALUES(?,?,NULL)",
            params![sql_id(parent), name],
        )?;
        namespace::changed(&tx, parent, name)?;
        namespace::count_delta(&tx, parent, -1)?;
        tx.execute(
            "UPDATE nodes SET nlink=CASE WHEN kind=2 THEN 0 ELSE nlink-1 END,ctime=? WHERE id=?",
            params![now(), sql_id(n.attr.ino)],
        )?;
        if directory {
            tx.execute(
                "UPDATE nodes SET nlink=nlink-1 WHERE id=?",
                [sql_id(parent)],
            )?;
        }
        Self::dirty(&tx, parent)?;
        observation::mutation(&tx, "unlink", n.attr.ino, Some(parent), Some(name), None)?;
        tx.commit()?;
        Ok(())
    }
    pub fn link(&self, id: u64, parent: u64, name: &[u8]) -> Result<VfsAttr> {
        name_ok(name)?;
        let mut c = self.db()?;
        let tx = c.transaction()?;
        mutable(&tx)?;
        let n = node(&tx, id)?;
        if n.attr.kind != NodeKind::File {
            return Err(error(libc::EPERM));
        }
        require_dir(&tx, parent)?;
        match self.lookup_tx(&tx, parent, name) {
            Ok(_) => return Err(error(libc::EEXIST)),
            Err(e) if errno(&e) == libc::ENOENT => {}
            Err(e) => return Err(e),
        }
        tx.execute(
            "INSERT OR REPLACE INTO dirents VALUES(?,?,?)",
            params![sql_id(parent), name, sql_id(id)],
        )?;
        namespace::changed(&tx, parent, name)?;
        namespace::count_delta(&tx, parent, 1)?;
        tx.execute(
            "UPDATE nodes SET nlink=nlink+1,ctime=? WHERE id=?",
            params![now(), sql_id(id)],
        )?;
        Self::dirty(&tx, parent)?;
        observation::mutation(&tx, "link", id, Some(parent), Some(name), None)?;
        let n = node(&tx, id)?;
        tx.commit()?;
        self.actual_attr(n)
    }
    /// flags0 or Linux RENAME_NOREPLACE/RENAME_EXCHANGE. No recursive copy fallback.
    pub fn rename(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
    ) -> Result<()> {
        name_ok(name)?;
        name_ok(newname)?;
        if flags & !3 != 0 || flags == 3 {
            return Err(error(libc::EINVAL));
        }
        let mut c = self.db()?;
        let tx = c.transaction()?;
        mutable(&tx)?;
        let src = self.lookup_tx(&tx, parent, name)?;
        require_dir(&tx, newparent)?;
        let dst = match self.lookup_tx(&tx, newparent, newname) {
            Ok(n) => Some(n),
            Err(e) if errno(&e) == libc::ENOENT => None,
            Err(e) => return Err(e),
        };
        if dst.as_ref().is_some_and(|d| d.attr.ino == src.attr.ino) {
            if flags == 1 {
                return Err(error(libc::EEXIST));
            }
            return Ok(());
        }
        if flags == 1 && dst.is_some() {
            return Err(error(libc::EEXIST));
        }
        if flags == 2 && dst.is_none() {
            return Err(error(libc::ENOENT));
        }
        let check_cycle = |moving: &Node, to: u64| -> Result<()> {
            if moving.attr.kind != NodeKind::Directory {
                return Ok(());
            }
            let mut p = to;
            for _ in 0..=MAX_DEPTH {
                if p == moving.attr.ino {
                    return Err(error(libc::EINVAL));
                }
                if p == 1 {
                    return Ok(());
                }
                p = node(&tx, p)?.attr.parent;
            }
            Err(error(libc::ELOOP))
        };
        check_cycle(&src, newparent)?;
        if let Some(d) = &dst {
            if flags == 2 {
                check_cycle(d, parent)?
            } else {
                if src.attr.kind == NodeKind::Directory && d.attr.kind != NodeKind::Directory {
                    return Err(error(libc::ENOTDIR));
                }
                if src.attr.kind != NodeKind::Directory && d.attr.kind == NodeKind::Directory {
                    return Err(error(libc::EISDIR));
                }
                if d.attr.kind == NodeKind::Directory && !self.empty(&tx, d.attr.ino)? {
                    return Err(error(libc::ENOTEMPTY));
                }
                tx.execute(
                    "UPDATE nodes SET nlink=CASE WHEN kind=2 THEN 0 ELSE nlink-1 END,ctime=? WHERE id=?",
                    params![now(), sql_id(d.attr.ino)],
                )?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO dirents VALUES(?,?,?)",
            params![sql_id(newparent), newname, sql_id(src.attr.ino)],
        )?;
        if flags == 2 {
            let d = dst.as_ref().unwrap();
            tx.execute(
                "INSERT OR REPLACE INTO dirents VALUES(?,?,?)",
                params![sql_id(parent), name, sql_id(d.attr.ino)],
            )?;
            if d.attr.kind == NodeKind::Directory {
                tx.execute(
                    "UPDATE nodes SET parent=? WHERE id=?",
                    params![sql_id(parent), sql_id(d.attr.ino)],
                )?;
            }
        } else {
            tx.execute(
                "INSERT OR REPLACE INTO dirents VALUES(?,?,NULL)",
                params![sql_id(parent), name],
            )?;
        }
        if src.attr.kind == NodeKind::Directory {
            tx.execute(
                "UPDATE nodes SET parent=?,ctime=? WHERE id=?",
                params![sql_id(newparent), now(), sql_id(src.attr.ino)],
            )?;
        }
        let src_dir = i64::from(src.attr.kind == NodeKind::Directory);
        let dst_dir = i64::from(
            dst.as_ref()
                .is_some_and(|d| d.attr.kind == NodeKind::Directory),
        );
        let old_delta = if flags == 2 {
            dst_dir - src_dir
        } else {
            -src_dir
        };
        let new_delta = if flags == 2 {
            src_dir - dst_dir
        } else {
            src_dir - dst_dir
        };
        tx.execute(
            "UPDATE nodes SET nlink=nlink+? WHERE id=?",
            params![old_delta, sql_id(parent)],
        )?;
        tx.execute(
            "UPDATE nodes SET nlink=nlink+? WHERE id=?",
            params![new_delta, sql_id(newparent)],
        )?;
        namespace::changed(&tx, parent, name)?;
        namespace::changed(&tx, newparent, newname)?;
        if flags != 2 {
            namespace::count_delta(&tx, parent, -1)?;
            if dst.is_none() {
                namespace::count_delta(&tx, newparent, 1)?;
            }
        }
        Self::dirty(&tx, parent)?;
        Self::dirty(&tx, newparent)?;
        observation::mutation(
            &tx,
            "rename",
            src.attr.ino,
            Some(parent),
            Some(name),
            Some((newparent, newname)),
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn entries(&self, parent: u64) -> Result<Vec<(Vec<u8>, VfsAttr)>> {
        let mut c = self.db()?;
        let tx = c.transaction()?;
        let names = self.names(&tx, parent)?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let n = self.lookup_tx(&tx, parent, &name)?;
            out.push((name, self.actual_attr(n)?));
        }
        tx.commit()?;
        Ok(out)
    }
    pub fn open_handle(&self, id: u64, flags: i32, directory: bool) -> Result<u64> {
        let a = self.attr(id)?;
        if directory != (a.kind == NodeKind::Directory) {
            return Err(error(if directory {
                libc::ENOTDIR
            } else {
                libc::EISDIR
            }));
        }
        if !directory && a.kind != NodeKind::File {
            return Err(error(libc::ELOOP));
        }
        let snapshot = if directory {
            let mut rows = vec![
                (b".".to_vec(), a.clone()),
                (b"..".to_vec(), self.attr(a.parent)?),
            ];
            if self.manager().0.remote.is_none() && flags & libc::O_NONBLOCK == 0 {
                rows.extend(self.entries(id)?);
            }
            Some(Arc::new(rows))
        } else {
            None
        };
        let h = self.0.next_handle.fetch_add(1, Ordering::Relaxed);
        self.0.handles.lock().map_err(|_| error(libc::EIO))?.insert(
            h,
            Handle {
                ino: id,
                flags,
                directory: snapshot,
                deferred_directory: directory
                    && (self.manager().0.remote.is_some() || flags & libc::O_NONBLOCK != 0),
            },
        );
        Ok(h)
    }
    pub fn check_handle(&self, id: u64, handle: u64, write: bool) -> Result<()> {
        if let Some(remote) = &self.manager().0.remote {
            remote.guard()?;
        }
        let hs = self.0.handles.lock().map_err(|_| error(libc::EIO))?;
        let h = hs.get(&handle).ok_or_else(|| error(libc::EBADF))?;
        if h.ino != id
            || h.directory.is_some()
            || write && (h.flags & libc::O_ACCMODE) == libc::O_RDONLY
            || !write && (h.flags & libc::O_ACCMODE) == libc::O_WRONLY
        {
            return Err(error(libc::EBADF));
        }
        Ok(())
    }
    pub fn directory_handle(&self, id: u64, handle: u64) -> Result<Arc<Vec<(Vec<u8>, VfsAttr)>>> {
        if self.manager().0.remote.is_some() {
            return Err(error(libc::EOPNOTSUPP))
                .context("remote directory handles require bounded read_directory_page");
        }
        let (snapshot, deferred) = {
            let hs = self.0.handles.lock().map_err(|_| error(libc::EIO))?;
            let h = hs.get(&handle).ok_or_else(|| error(libc::EBADF))?;
            ensure!(h.ino == id, "handle inode mismatch");
            (
                h.directory.clone().context("not a directory handle")?,
                h.deferred_directory,
            )
        };
        if !deferred {
            return Ok(snapshot);
        }
        let mut rows = (*snapshot).clone();
        rows.extend(self.entries(id)?);
        let rows = Arc::new(rows);
        if let Some(remote) = &self.manager().0.remote {
            remote.guard()?;
        }
        let mut hs = self.0.handles.lock().map_err(|_| error(libc::EIO))?;
        let h = hs.get_mut(&handle).ok_or_else(|| error(libc::EBADF))?;
        ensure!(h.ino == id, "handle inode mismatch");
        if h.deferred_directory {
            h.directory = Some(rows);
            h.deferred_directory = false;
        }
        h.directory.clone().context("directory handle disappeared")
    }
    /// Remote snapshots freeze on first readdir. Cookies are opaque per-open
    /// handle, not byte offsets. Local handles retain their original eager snapshot.
    pub fn read_directory_page(
        &self,
        id: u64,
        handle: u64,
        cookie: u64,
        max_entries: usize,
    ) -> Result<DirectoryPage> {
        if max_entries == 0 || max_entries > readdir::DIRECTORY_PAGE_ENTRIES {
            return Err(error(libc::EINVAL));
        }
        if let Some(pager) = &self.0.pager {
            let mut c = self.db()?;
            let handles = self.0.handles.lock().map_err(|_| error(libc::EIO))?;
            let h = handles.get(&handle).ok_or_else(|| error(libc::EBADF))?;
            if h.ino != id || h.directory.is_none() {
                return Err(error(libc::EBADF));
            }
            return pager.lock().map_err(|_| error(libc::EIO))?.page(
                self,
                &mut c,
                handle,
                id,
                cookie,
                max_entries,
            );
        }
        let rows = self.directory_handle(id, handle)?;
        let skip = usize::try_from(cookie).unwrap_or(usize::MAX);
        let entries = rows
            .iter()
            .enumerate()
            .skip(skip)
            .take(max_entries)
            .map(|(i, (n, a))| DirectoryListingEntry {
                name: n.clone(),
                ino: a.ino,
                kind: a.kind,
                next_cookie: i as u64 + 1,
            })
            .collect::<Vec<_>>();
        let eof = skip.saturating_add(entries.len()) >= rows.len();
        Ok(DirectoryPage { entries, eof })
    }
    pub fn readdir_stats(&self) -> Result<Option<ReaddirStats>> {
        let c = self.db()?;
        self.0
            .pager
            .as_ref()
            .map(|p| p.lock().map_err(|_| error(libc::EIO))?.stats(&c))
            .transpose()
    }
    pub fn close_handle(&self, id: u64, handle: u64) -> Result<()> {
        let mut hs = self.0.handles.lock().map_err(|_| error(libc::EIO))?;
        ensure!(
            hs.get(&handle).is_some_and(|h| h.ino == id),
            "invalid handle"
        );
        if let Some(pager) = &self.0.pager {
            pager.lock().map_err(|_| error(libc::EIO))?.close(handle)?;
        }
        hs.remove(&handle);
        Ok(())
    }
    pub fn sync(&self, id: u64) -> Result<()> {
        let c = self.db()?;
        let n = node(&c, id)?;
        if let Some(b) = n.backing {
            self.private_file(&b, true)?.sync_all()?;
            File::open(self.0.path.join("files"))?.sync_all()?;
        }
        c.execute_batch("PRAGMA wal_checkpoint(FULL);")?;
        Ok(())
    }
    /// Requires no mounted kernel connection. The Linux adapter clears this
    /// guard only after successful unmount+join; live checkpoints are rejected.
    /// Direct logical API callers must stop their own handle writers first.
    pub fn checkpoint_unmounted(&self) -> Result<ObjectRef> {
        let snapshot = self.checkpoint_snapshot_unmounted()?;
        let topology: HardlinkTopology =
            serde_json::from_slice(&self.manager().store().read_object(&snapshot.hardlinks)?)?;
        ensure!(
            topology.groups.is_empty(),
            "plain Directory root omits hardlink topology; use VfsCheckpoint"
        );
        Ok(snapshot.directory)
    }
    pub fn checkpoint_snapshot_unmounted(&self) -> Result<VfsCheckpoint> {
        self.require_local_checkpoint()?;
        ensure!(
            !self.0.mounted.load(Ordering::Acquire),
            "checkpoint requires a detached/drained FUSE session"
        );
        ensure!(
            self.0
                .handles
                .lock()
                .map_err(|_| error(libc::EIO))?
                .is_empty(),
            "checkpoint requires closed logical handles"
        );
        let mut c = self.db()?;
        ensure!(
            !self.0.mounted.load(Ordering::Acquire),
            "FUSE mount raced checkpoint"
        );
        let tx = c.transaction()?;
        let lease = self
            .manager()
            .store()
            .begin_staging(Duration::from_secs(3600))?;
        let result = (|| {
            let captured = if self.0.v2 {
                Some(namespace::capture(
                    self,
                    &tx,
                    &lease.id,
                    &ImportPolicy::default(),
                )?)
            } else {
                None
            };
            let (root, topology) = if let Some(captured) = &captured {
                (captured.directory.clone(), namespace::topology(self, &tx)?)
            } else {
                let mut seen = BTreeSet::new();
                let mut linked = BTreeMap::<u64, u64>::new();
                let root = self.checkpoint_dir(&tx, 1, 0, &mut seen, &mut linked)?;
                let mut groups = BTreeMap::<u64, Vec<Vec<Vec<u8>>>>::new();
                self.collect_topology(&tx, 1, &mut Vec::new(), &mut groups)?;
                let mut groups = groups
                    .into_values()
                    .filter(|p| p.len() > 1)
                    .collect::<Vec<_>>();
                for g in &mut groups {
                    g.sort();
                }
                groups.sort();
                let topology = HardlinkTopology {
                    format_version: 1,
                    groups: groups
                        .into_iter()
                        .map(|paths| HardlinkGroup {
                            paths_hex: paths
                                .iter()
                                .map(|p| p.iter().map(hex::encode).collect())
                                .collect(),
                        })
                        .collect(),
                };
                (root, topology)
            };
            let hardlinks = self
                .manager()
                .store()
                .put_blob(&canonical_json(&topology)?)?;
            // Retain topology first. publish_extending atomically transfers
            // staging to the directory pin, pruning already retained blobs.
            self.manager().store().pin(
                &format!("vfs:view:{}:topology:{}", self.id(), hardlinks.digest),
                &hardlinks,
            )?;
            let mut anchors = vec![node(&tx, 1)?.base.context("root without retained base")?];
            if let Some(latest) = tx
                .query_row("SELECT value FROM meta WHERE key='latest'", [], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
            {
                let latest = serde_json::from_str::<ObjectRef>(&latest)?;
                if !anchors.contains(&latest) {
                    anchors.push(latest);
                }
            }
            self.manager().store().publish_extending(
                &lease.id,
                &format!("vfs:view:{}:checkpoint:{}", self.id(), root.digest),
                &root,
                &anchors,
            )?;
            if let Some(captured) = &captured {
                namespace::rebase(&tx, captured)?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO meta VALUES('latest',?)",
                [json_ref(&root)?],
            )?;
            let snapshot = VfsCheckpoint {
                format_version: 1,
                directory: root,
                hardlinks,
            };
            tx.execute(
                "INSERT OR REPLACE INTO meta VALUES('latest_snapshot',?)",
                [serde_json::to_string(&snapshot)?],
            )?;
            tx.commit()?;
            Ok(snapshot)
        })();
        let cleanup = self.manager().store().abandon_staging(&lease.id);
        match result {
            Ok(value) => {
                cleanup?;
                Ok(value)
            }
            Err(e) => Err(e),
        }
    }
    pub fn seal(&self) -> Result<VfsCheckpoint> {
        self.require_local_checkpoint()?;
        {
            let c = self.db()?;
            ensure!(
                !self.0.mounted.load(Ordering::Acquire),
                "seal requires a drained FUSE session"
            );
            ensure!(
                self.0
                    .handles
                    .lock()
                    .map_err(|_| error(libc::EIO))?
                    .is_empty(),
                "seal requires closed handles"
            );
            c.execute("INSERT OR REPLACE INTO meta VALUES('sealed','true')", [])?;
        }
        self.checkpoint_snapshot_unmounted()
    }
    /// Logical destruction retains private bytes and all published roots. A
    /// separate retention-aware sweeper is deliberately not implemented yet.
    pub fn destroy(&self) -> Result<VfsCheckpoint> {
        let snapshot = self.seal()?;
        self.db()?
            .execute("INSERT OR REPLACE INTO meta VALUES('destroyed','true')", [])?;
        Ok(snapshot)
    }
    fn collect_topology(
        &self,
        c: &Connection,
        id: u64,
        prefix: &mut Vec<Vec<u8>>,
        groups: &mut BTreeMap<u64, Vec<Vec<Vec<u8>>>>,
    ) -> Result<()> {
        ensure!(prefix.len() <= MAX_DEPTH, "topology depth limit");
        for name in self.names(c, id)? {
            let n = self.lookup_tx(c, id, &name)?;
            prefix.push(name);
            if n.attr.kind == NodeKind::Directory {
                self.collect_topology(c, n.attr.ino, prefix, groups)?
            } else if n.attr.kind == NodeKind::File && n.attr.nlink > 1 {
                groups.entry(n.attr.ino).or_default().push(prefix.clone());
            }
            prefix.pop();
        }
        Ok(())
    }
    fn checkpoint_dir(
        &self,
        c: &Connection,
        id: u64,
        depth: usize,
        seen: &mut BTreeSet<u64>,
        links: &mut BTreeMap<u64, u64>,
    ) -> Result<ObjectRef> {
        ensure!(
            depth <= MAX_DEPTH && seen.insert(id),
            "directory cycle/depth limit"
        );
        let n = require_dir(c, id)?;
        // Cached link() operations may point at untouched files; inspect only
        // private dirent identities to reject topology loss, not inherited bytes.
        if !n.dirty {
            seen.remove(&id);
            return n.base.context("clean directory has no base");
        }
        let mut entries = Vec::new();
        for name in self.names(c, id)? {
            let child = self.lookup_tx(c, id, &name)?;
            let object = match child.attr.kind {
                NodeKind::Directory => EntryNode::Directory {
                    object: self.checkpoint_dir(c, child.attr.ino, depth + 1, seen, links)?,
                },
                NodeKind::File => {
                    *links.entry(child.attr.ino).or_default() += 1;
                    let object = if let Some(b) = child.backing {
                        let mut f = self.private_file(&b, true)?;
                        f.sync_all()?;
                        let mut bytes = Vec::new();
                        f.read_to_end(&mut bytes)?;
                        self.manager().store().put_blob(&bytes)?
                    } else {
                        child.base.context("file without base")?
                    };
                    EntryNode::File {
                        object,
                        mode: child.attr.mode,
                    }
                }
                NodeKind::Symlink => EntryNode::Symlink {
                    target_hex: hex::encode(child.target),
                },
            };
            entries.push(DirectoryEntry::new(&name, object)?);
        }
        seen.remove(&id);
        self.manager().store().put_directory(DirectoryManifest {
            format_version: FORMAT_VERSION,
            mode: n.attr.mode,
            entries,
        })
    }
}

pub fn errno(e: &anyhow::Error) -> i32 {
    if let Some(code) = e
        .downcast_ref::<std::io::Error>()
        .and_then(|e| e.raw_os_error())
    {
        return code;
    }
    e.chain()
        .find_map(|e| {
            e.downcast_ref::<std::io::Error>()
                .and_then(|e| e.raw_os_error())
        })
        .unwrap_or(libc::EIO)
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn immutable_fd_admission_is_shared_and_bounded_even_while_readers_hold_arcs() {
        let base = std::env::var_os("RUDDER_TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if Path::new("/workspace").is_dir() {
                    "/workspace".into()
                } else {
                    std::env::temp_dir()
                }
            });
        let temporary = tempfile::Builder::new()
            .prefix("rudder-vfs-fds-")
            .tempdir_in(base)
            .unwrap();
        let store = LocalStore::open(temporary.path().join("cas")).unwrap();
        let manager = VfsManager::open(temporary.path().join("manager"), store.clone()).unwrap();
        let root = store
            .put_directory(DirectoryManifest {
                format_version: 1,
                mode: 0o755,
                entries: vec![],
            })
            .unwrap();
        manager.prepare(&root).unwrap();
        let owner = OwnerEpoch {
            owner: "cache-test".into(),
            epoch: 1,
        };
        let first = manager
            .fork(&Uuid::new_v4().to_string(), &root, owner.clone())
            .unwrap();
        let second = manager
            .fork(&Uuid::new_v4().to_string(), &root, owner)
            .unwrap();
        let references = (0..=MAX_IMMUTABLE_FDS)
            .map(|number| store.put_blob(&(number as u64).to_le_bytes()).unwrap())
            .collect::<Vec<_>>();
        let mut held = references[..MAX_IMMUTABLE_FDS]
            .iter()
            .map(|reference| first.inherited_file(reference).unwrap())
            .collect::<Vec<_>>();
        assert!(Arc::ptr_eq(
            &held[0],
            &second.inherited_file(&references[0]).unwrap()
        ));
        assert_eq!(
            errno(
                &second
                    .inherited_file(references.last().unwrap())
                    .unwrap_err()
            ),
            libc::EAGAIN
        );
        held.pop();
        assert!(second.inherited_file(references.last().unwrap()).is_ok());
        assert_eq!(
            manager.0.verified.lock().unwrap().0.len(),
            MAX_IMMUTABLE_FDS
        );
    }
}
