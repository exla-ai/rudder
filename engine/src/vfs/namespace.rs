//! Namespace support for opt-in paged/range views. Legacy views keep their
//! prepared SQLite directory index and existing wire/data interpretation.
use super::observation::WorkMeter;
use super::*;
use crate::directory::DirectoryStore;
use crate::directory::{
    self as paged, EntryNode as PagedNode, LocalDirectoryStore, PagedDirectory,
};

struct MeteredDirectory<'a> {
    inner: LocalDirectoryStore<'a>,
    meter: Option<&'a WorkMeter<'a>>,
}
impl DirectoryStore for MeteredDirectory<'_> {
    fn read(&self, object: &ObjectRef) -> Result<Vec<u8>> {
        if let Some(meter) = self.meter {
            meter.metadata(object.size_bytes)?;
        }
        self.inner.read(object)
    }
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        if let Some(meter) = self.meter {
            meter.metadata(bytes.len() as u64)?;
        }
        self.inner.put(kind, bytes)
    }
}

pub(super) fn header(store: &LocalStore, root: &ObjectRef) -> Result<(u32, u64, u64)> {
    let root = crate::tree::root_info(store, root)?;
    Ok((root.mode, root.entry_count, root.subdirectory_count))
}
pub(super) fn lookup(
    store: &LocalStore,
    root: &ObjectRef,
    name: &[u8],
) -> Result<Option<paged::DirectoryEntry>> {
    crate::tree::lookup(store, root, name)
}
pub(super) fn header_metered(
    store: &LocalStore,
    root: &ObjectRef,
    meter: Option<&WorkMeter<'_>>,
) -> Result<(u32, u64, u64)> {
    if let Some(meter) = meter {
        meter.metadata(root.size_bytes)?;
    }
    header(store, root)
}
pub(super) fn lookup_metered(
    store: &LocalStore,
    root: &ObjectRef,
    name: &[u8],
    meter: Option<&WorkMeter<'_>>,
) -> Result<Option<paged::DirectoryEntry>> {
    let Some(meter) = meter else {
        return lookup(store, root, name);
    };
    meter.metadata(root.size_bytes)?;
    let bytes = store.read_object(root)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value.get("format_version").and_then(|v| v.as_u64()) == Some(2) {
        let adapter = MeteredDirectory {
            inner: LocalDirectoryStore::read_only(store),
            meter: Some(meter),
        };
        PagedDirectory::new(&adapter).lookup(root, name)
    } else {
        meter.metadata(root.size_bytes)?;
        lookup(store, root, name)
    }
}
pub(super) fn view_lookup(
    view: &VfsView,
    parent: &Node,
    root: &ObjectRef,
    name: &[u8],
    meter: Option<&WorkMeter<'_>>,
) -> Result<Option<paged::DirectoryEntry>> {
    if let Some(remote) = &view.manager().0.remote {
        return remote.lookup(parent.origin.as_deref(), root, name);
    }
    lookup_metered(view.manager().store(), root, name, meter)
}
pub(super) fn view_header(
    view: &VfsView,
    origin: Option<&str>,
    root: &ObjectRef,
    meter: Option<&WorkMeter<'_>>,
) -> Result<(u32, u64, u64)> {
    if let Some(remote) = &view.manager().0.remote {
        return remote.header(origin, root);
    }
    header_metered(view.manager().store(), root, meter)
}
fn base_names(store: &LocalStore, root: &ObjectRef) -> Result<Vec<Vec<u8>>> {
    let mut after = None;
    let mut names = Vec::new();
    loop {
        let page = crate::tree::scan(store, root, after.as_deref(), 1024)?;
        for e in page.entries {
            names.push(e.name()?)
        }
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    Ok(names)
}
pub(super) fn initialize(c: &Connection, root_entries: u64) -> Result<()> {
    ensure!(
        root_entries <= i64::MAX as u64,
        "directory count exceeds private namespace capacity"
    );
    c.execute_batch("CREATE TABLE directory_sizes(inode INTEGER PRIMARY KEY,entry_count INTEGER NOT NULL CHECK(entry_count>=0));CREATE TABLE dirty_dirents(parent INTEGER NOT NULL,name BLOB NOT NULL,PRIMARY KEY(parent,name)) WITHOUT ROWID;")?;
    c.execute(
        "INSERT INTO directory_sizes VALUES(1,?)",
        [sql_id(root_entries)],
    )?;
    Ok(())
}
pub(super) fn enabled(c: &Connection) -> Result<bool> {
    Ok(c.query_row(
        "SELECT value IN ('2','3','4') FROM meta WHERE key='format_version'",
        [],
        |r| r.get(0),
    )?)
}
pub(super) fn changed(c: &Connection, parent: u64, name: &[u8]) -> Result<()> {
    if enabled(c)? {
        c.execute(
            "INSERT OR IGNORE INTO dirty_dirents VALUES(?,?)",
            params![sql_id(parent), name],
        )?;
    }
    Ok(())
}
pub(super) fn bindings_changed(c: &Connection, id: u64) -> Result<()> {
    if enabled(c)? {
        c.execute(
            "INSERT OR IGNORE INTO dirty_dirents SELECT parent,name FROM dirents WHERE child=?",
            [sql_id(id)],
        )?;
    }
    Ok(())
}
pub(super) fn count_delta(c: &Connection, parent: u64, delta: i64) -> Result<()> {
    if !enabled(c)? {
        return Ok(());
    }
    let count = c.query_row(
        "SELECT entry_count FROM directory_sizes WHERE inode=?",
        [sql_id(parent)],
        |r| read_id(r, 0),
    )?;
    let count = if delta >= 0 {
        count.checked_add(delta as u64)
    } else {
        count.checked_sub(delta.unsigned_abs())
    }
    .context("directory entry count overflow/underflow")?;
    ensure!(
        count <= i64::MAX as u64,
        "directory entry count exceeds private namespace capacity"
    );
    c.execute(
        "UPDATE directory_sizes SET entry_count=? WHERE inode=?",
        params![sql_id(count), sql_id(parent)],
    )?;
    Ok(())
}
pub(super) fn names(view: &VfsView, c: &Connection, parent: u64) -> Result<Vec<Vec<u8>>> {
    let n = require_dir(c, parent)?;
    let mut names = if let Some(base) = n.base {
        (if let Some(remote) = &view.manager().0.remote {
            remote.names(n.origin.as_deref(), &base)?
        } else {
            base_names(view.manager().store(), &base)?
        })
        .into_iter()
        .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    let mut q =
        c.prepare("SELECT name,child IS NOT NULL FROM dirents WHERE parent=? ORDER BY name")?;
    for row in q.query_map([sql_id(parent)], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, bool>(1)?))
    })? {
        let (name, present) = row?;
        if present {
            names.insert(name);
        } else {
            names.remove(&name);
        }
    }
    Ok(names.into_iter().collect())
}
pub(super) fn ensure_paged(
    store: &LocalStore,
    stage: &str,
    base: Option<&ObjectRef>,
    mode: u32,
) -> Result<ObjectRef> {
    let adapter = LocalDirectoryStore::new(store, stage);
    let tree = PagedDirectory::new(&adapter);
    let Some(base) = base else {
        return tree.build(mode, []);
    };
    crate::tree::to_paged_directory(store, stage, base)
}
fn ensure_paged_metered(
    store: &LocalStore,
    stage: &str,
    base: Option<&ObjectRef>,
    mode: u32,
    meter: Option<&WorkMeter<'_>>,
) -> Result<ObjectRef> {
    let Some(meter) = meter else {
        return ensure_paged(store, stage, base, mode);
    };
    let adapter = MeteredDirectory {
        inner: LocalDirectoryStore::new(store, stage),
        meter: Some(meter),
    };
    let tree = PagedDirectory::new(&adapter);
    let Some(base) = base else {
        return tree.build(mode, []);
    };
    meter.metadata(base.size_bytes)?;
    let bytes = store.read_object(base)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value.get("format_version").and_then(|v| v.as_u64()) == Some(2) {
        paged::decode_directory_root(&bytes)?;
        return Ok(base.clone());
    }
    let legacy: DirectoryManifest = serde_json::from_slice(&bytes)?;
    let mut entries = Vec::with_capacity(legacy.entries.len());
    for entry in legacy.entries {
        meter.entry()?;
        let name = entry.name()?;
        let node = match entry.node {
            EntryNode::File { object, mode } => PagedNode::File {
                logical_size: object.size_bytes,
                object,
                mode,
            },
            EntryNode::Directory { object } => PagedNode::Directory { object },
            EntryNode::Symlink { target_hex } => PagedNode::Symlink { target_hex },
        };
        entries.push(paged::DirectoryEntry::new(&name, node)?);
    }
    tree.build(legacy.mode, entries)
}

pub(super) fn retained_anchors(view: &VfsView, c: &Connection) -> Result<Vec<ObjectRef>> {
    let mut anchors = vec![node(c, 1)?.base.context("root without retained baseline")?];
    let original = c.query_row("SELECT value FROM meta WHERE key='base'", [], |r| {
        r.get::<_, String>(0)
    })?;
    let original: ObjectRef = serde_json::from_str(&original)?;
    if !anchors.contains(&original) {
        anchors.push(original);
    }
    if let Some(latest) = c
        .query_row("SELECT value FROM meta WHERE key='latest'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
    {
        let latest = serde_json::from_str::<ObjectRef>(&latest)?;
        if !anchors.contains(&latest) {
            anchors.push(latest)
        }
    }
    // Each is separately pinned by prepared/view/checkpoint publication. The
    // store checks that authority atomically with the new root publication.
    let _ = view;
    Ok(anchors)
}

impl VfsManager {
    /// Explicit opt-in. Existing public prepare()/fork() retain legacy behavior.
    /// A new root is fully retained once; an existing retained root is repinned.
    /// Paged lookup needs no all-directory SQLite entry index.
    pub fn prepare_paged(&self, root: &ObjectRef) -> Result<()> {
        if self.0.remote.is_some() {
            return Err(error(libc::EOPNOTSUPP))
                .context("remote roots cannot acquire complete LocalStore pins");
        }
        let (mode, entries, subdirectories) = header(self.store(), root)?;
        ensure!(
            entries <= i64::MAX as u64
                && subdirectories
                    .checked_add(2)
                    .is_some_and(|v| v <= i64::MAX as u64),
            "directory counts exceed private namespace capacity"
        );
        let pin = format!("vfs:paged-prepared:{}", root.digest);
        if self.store().retention()?.pins.values().any(|r| r == root) {
            self.store().pin_existing(&pin, root)?;
        } else {
            self.store().pin(&pin, root)?;
        }
        let index = self.0.index.lock().map_err(|_| error(libc::EIO))?;
        index.execute(
            "INSERT OR REPLACE INTO prepared_paged VALUES(?,?,?,?,?)",
            params![
                root.digest,
                json_ref(root)?,
                mode,
                sql_id(entries),
                sql_id(subdirectories)
            ],
        )?;
        Ok(())
    }
    pub fn fork_paged(&self, id: &str, root: &ObjectRef, owner: OwnerEpoch) -> Result<VfsView> {
        self.fork_inner(id, root, None, owner, true)
    }
    /// Preserves declared real hardlinks; plain roots still mean independent files.
    pub fn prepare_paged_snapshot(&self, snapshot: &VfsCheckpoint) -> Result<()> {
        if self.0.remote.is_some() {
            return Err(error(libc::EOPNOTSUPP))
                .context("remote snapshots require explicit partial-root retention");
        }
        let topology = validate_snapshot(self.store(), snapshot)?;
        self.prepare_paged(&snapshot.directory)?;
        let mut index = self.0.index.lock().map_err(|_| error(libc::EIO))?;
        let tx = index.transaction()?;
        for group in &topology.groups {
            let group_id = encode_path(&decode_path(&group.paths_hex[0])?);
            for raw in &group.paths_hex {
                tx.execute(
                    "INSERT OR REPLACE INTO topology_paths VALUES(?,?,?,?)",
                    params![
                        snapshot.hardlinks.digest,
                        encode_path(&decode_path(raw)?),
                        group_id,
                        group.paths_hex.len() as i64
                    ],
                )?;
            }
        }
        let pin = format!("vfs:topology:{}", snapshot.hardlinks.digest);
        if self
            .store()
            .retention()?
            .pins
            .values()
            .any(|r| r == &snapshot.hardlinks)
        {
            self.store().pin_existing(&pin, &snapshot.hardlinks)?;
        } else {
            self.store().pin(&pin, &snapshot.hardlinks)?;
        }
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
    pub fn fork_paged_snapshot(
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
            true,
        )
    }
}

pub(super) struct Captured {
    pub directory: ObjectRef,
    pub files: BTreeMap<u64, ObjectRef>,
    pub directories: BTreeMap<u64, ObjectRef>,
}
pub(super) fn capture(
    view: &VfsView,
    c: &Connection,
    stage: &str,
    policy: &ImportPolicy,
) -> Result<Captured> {
    capture_metered(view, c, stage, policy, None)
}
pub(super) fn capture_metered(
    view: &VfsView,
    c: &Connection,
    stage: &str,
    policy: &ImportPolicy,
    meter: Option<&WorkMeter<'_>>,
) -> Result<Captured> {
    view.require_local_checkpoint()?;
    capture_inner(view, c, stage, policy, meter, None)
}
pub(super) fn capture_partial_metered<'a>(
    view: &'a VfsView,
    c: &'a Connection,
    stage: &'a str,
    policy: &'a ImportPolicy,
    meter: &'a WorkMeter<'a>,
    collector: &'a capture::Collector<'a>,
) -> Result<Captured> {
    capture_inner(view, c, stage, policy, Some(meter), Some(collector))
}
fn capture_inner<'a>(
    view: &'a VfsView,
    c: &'a Connection,
    stage: &'a str,
    policy: &'a ImportPolicy,
    meter: Option<&'a WorkMeter<'a>>,
    collector: Option<&'a capture::Collector<'a>>,
) -> Result<Captured> {
    let mut capture = Capture {
        view,
        c,
        stage,
        policy,
        meter,
        collector,
        files: BTreeMap::new(),
        directories: BTreeMap::new(),
        seen: BTreeSet::new(),
    };
    let directory = capture.directory(1, &mut Vec::new())?;
    Ok(Captured {
        directory,
        files: capture.files,
        directories: capture.directories,
    })
}
struct Capture<'a> {
    view: &'a VfsView,
    c: &'a Connection,
    stage: &'a str,
    policy: &'a ImportPolicy,
    meter: Option<&'a WorkMeter<'a>>,
    collector: Option<&'a capture::Collector<'a>>,
    files: BTreeMap<u64, ObjectRef>,
    directories: BTreeMap<u64, ObjectRef>,
    seen: BTreeSet<u64>,
}
impl Capture<'_> {
    fn directory(&mut self, id: u64, path: &mut Vec<Vec<u8>>) -> Result<ObjectRef> {
        if let Some(meter) = self.meter {
            meter.check()?;
        }
        ensure!(
            path.len() <= MAX_DEPTH && self.seen.insert(id),
            "directory capture cycle/depth limit"
        );
        let n = require_dir(self.c, id)?;
        if let Some(collector) = self.collector {
            collector.bind(&n)?;
        }
        let policy_relevant = self
            .policy
            .excluded_paths
            .iter()
            .any(|prefix| prefix.starts_with(path));
        if !n.dirty && !policy_relevant {
            if let Some(base) = n.base {
                self.seen.remove(&id);
                self.directories.insert(id, base.clone());
                return Ok(base);
            }
        }
        let adapter = MeteredDirectory {
            inner: LocalDirectoryStore::new(self.view.manager().store(), self.stage),
            meter: self.meter,
        };
        let store: &dyn DirectoryStore = match self.collector {
            Some(collector) => collector,
            None => &adapter,
        };
        let tree = PagedDirectory::new(store);
        let mut root = if self.collector.is_some() {
            match &n.base {
                Some(base) => base.clone(),
                None => tree.build(n.attr.mode, std::iter::empty())?,
            }
        } else {
            ensure_paged_metered(
                self.view.manager().store(),
                self.stage,
                n.base.as_ref(),
                n.attr.mode,
                self.meter,
            )?
        };
        let mut names = BTreeSet::new();
        let mut q = self
            .c
            .prepare("SELECT name FROM dirty_dirents WHERE parent=? ORDER BY name")?;
        for row in q.query_map([sql_id(id)], |r| r.get::<_, Vec<u8>>(0))? {
            let name = row?;
            if let Some(meter) = self.meter {
                meter.entry()?;
                meter.metadata(name.len() as u64)?;
            }
            names.insert(name);
        }
        drop(q);
        for prefix in &self.policy.excluded_paths {
            if prefix.starts_with(path) && prefix.len() > path.len() {
                let name = &prefix[path.len()];
                if !names.contains(name) {
                    if let Some(meter) = self.meter {
                        meter.entry()?;
                        meter.metadata(name.len() as u64)?;
                    }
                }
                names.insert(name.clone());
            }
        }
        for name in names {
            path.push(name.clone());
            let excluded = self
                .policy
                .excluded_paths
                .iter()
                .any(|prefix| path.starts_with(prefix));
            let replacement = if excluded {
                None
            } else {
                let child = match self.view.lookup_tx_metered(self.c, id, &name, self.meter) {
                    Ok(n) => Some(n),
                    Err(e) if errno(&e) == libc::ENOENT => None,
                    Err(e) => return Err(e),
                };
                match child {
                    None => None,
                    Some(child) => Some(match child.attr.kind {
                        NodeKind::Directory => PagedNode::Directory {
                            object: self.directory(child.attr.ino, path)?,
                        },
                        NodeKind::File => {
                            let object = if let Some(object) = self.files.get(&child.attr.ino) {
                                object.clone()
                            } else {
                                let object = match self.collector {
                                    Some(collector) => content::seal_collected(
                                        self.view,
                                        self.c,
                                        &child,
                                        self.stage,
                                        self.meter,
                                        Some(collector),
                                    )?,
                                    None => content::seal_metered(
                                        self.view, self.c, &child, self.stage, self.meter,
                                    )?,
                                };
                                self.files.insert(child.attr.ino, object.clone());
                                object
                            };
                            PagedNode::File {
                                object,
                                logical_size: child.attr.size,
                                mode: child.attr.mode,
                            }
                        }
                        NodeKind::Symlink => PagedNode::Symlink {
                            target_hex: hex::encode(child.target),
                        },
                    }),
                }
            };
            path.pop();
            root = tree.update(&root, &name, replacement)?.root;
        }
        root = tree.with_mode(&root, n.attr.mode)?;
        if self.policy.excluded_paths.is_empty() {
            let actual = tree.read_root(&root)?;
            let expected = self.c.query_row(
                "SELECT entry_count FROM directory_sizes WHERE inode=?",
                [sql_id(id)],
                |r| read_id(r, 0),
            )?;
            ensure!(
                actual.entry_count == expected
                    && actual.subdirectory_count.checked_add(2) == Some(n.attr.nlink),
                "captured directory counts differ from private namespace"
            );
        }
        self.seen.remove(&id);
        self.directories.insert(id, root.clone());
        Ok(root)
    }
}
/// Only a terminal/drained capture may change the mutable view's baseline.
/// Private logs are retained; clearing maps never discards the only copy.
pub(super) fn rebase(c: &Connection, captured: &Captured) -> Result<()> {
    ensure!(
        !c.is_autocommit(),
        "capture rebase requires a durable transaction"
    );
    for (inode, reference) in &captured.files {
        c.execute(
            "UPDATE nodes SET base=?,dirty=0 WHERE id=?",
            params![json_ref(reference)?, sql_id(*inode)],
        )?;
        c.execute(
            "UPDATE file_overlays SET base_limit=(SELECT size FROM nodes WHERE id=?) WHERE inode=?",
            params![sql_id(*inode), sql_id(*inode)],
        )?;
        c.execute("DELETE FROM file_blocks WHERE inode=?", [sql_id(*inode)])?;
    }
    for (inode, reference) in &captured.directories {
        c.execute(
            "UPDATE nodes SET base=?,dirty=0 WHERE id=?",
            params![json_ref(reference)?, sql_id(*inode)],
        )?;
    }
    c.execute("DELETE FROM dirty_dirents", [])?;
    Ok(())
}

fn directory_path(c: &Connection, mut inode: u64) -> Result<Option<Vec<Vec<u8>>>> {
    let mut reversed = Vec::new();
    let mut seen = BTreeSet::new();
    for _ in 0..=MAX_DEPTH {
        ensure!(seen.insert(inode), "current directory parent cycle");
        let n = require_dir(c, inode)?;
        if n.attr.nlink == 0 {
            return Ok(None);
        }
        if inode == 1 {
            reversed.reverse();
            return Ok(Some(reversed));
        }
        let mut q =
            c.prepare("SELECT name FROM dirents WHERE parent=? AND child=? ORDER BY name LIMIT 2")?;
        let names = q
            .query_map(params![sql_id(n.attr.parent), sql_id(inode)], |r| {
                r.get::<_, Vec<u8>>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if names.is_empty() {
            return Ok(None);
        }
        ensure!(
            names.len() == 1,
            "directory has multiple current names/parents"
        );
        reversed.push(names[0].clone());
        inode = n.attr.parent;
    }
    bail!("current directory depth limit")
}
/// Original paths identify lineage, not current names. Start from the deepest
/// reachable original directory so a separately moved child survives removal
/// of its former ancestor without remapping descendant paths during rename.
fn original_lookup(view: &VfsView, c: &Connection, path: &[Vec<u8>]) -> Result<Option<Node>> {
    ensure!(
        !path.is_empty() && path.len() <= MAX_DEPTH,
        "invalid original path"
    );
    let mut parent = 1;
    let mut consumed = 0;
    for depth in (1..path.len()).rev() {
        let origin = encode_path(&path[..depth]);
        let mut q = c.prepare("SELECT id FROM nodes WHERE origin=? AND kind=2 AND nlink>0")?;
        let ids = q
            .query_map([origin], |r| read_id(r, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        let mut found = None;
        for id in ids {
            if directory_path(c, id)?.is_some() {
                ensure!(found.is_none(), "original directory lineage is ambiguous");
                found = Some(id)
            }
        }
        if let Some(id) = found {
            parent = id;
            consumed = depth;
            break;
        }
    }
    for index in consumed..path.len() - 1 {
        let n = match view.lookup_tx(c, parent, &path[index]) {
            Ok(n) => n,
            Err(e) if errno(&e) == libc::ENOENT => return Ok(None),
            Err(e) => return Err(e),
        };
        if n.attr.kind != NodeKind::Directory
            || n.origin.as_deref() != Some(&encode_path(&path[..=index]))
        {
            return Ok(None);
        }
        parent = n.attr.ino;
    }
    match view.lookup_tx(c, parent, path.last().unwrap()) {
        Ok(n) => Ok(Some(n)),
        Err(e) if errno(&e) == libc::ENOENT => Ok(None),
        Err(e) => Err(e),
    }
}
pub(super) fn hydrate_links(view: &VfsView, c: &Connection, inode: u64) -> Result<()> {
    let group = c
        .query_row(
            "SELECT group_id FROM inherited_groups WHERE inode=?",
            [sql_id(inode)],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let Some(group) = group else { return Ok(()) };
    let topology = c.query_row("SELECT value FROM meta WHERE key='topology'", [], |r| {
        r.get::<_, String>(0)
    })?;
    let topology: ObjectRef = serde_json::from_str(&topology)?;
    let mut q = c.prepare(
        "SELECT path FROM inherited.topology_paths WHERE digest=? AND group_id=? ORDER BY path",
    )?;
    let paths = q
        .query_map(params![topology.digest, group], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(q);
    for path in paths {
        let path = path
            .split('/')
            .map(hex::decode)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let _ = original_lookup(view, c, &path)?;
    }
    Ok(())
}
pub(super) fn topology(view: &VfsView, c: &Connection) -> Result<HardlinkTopology> {
    let reference = c
        .query_row("SELECT value FROM meta WHERE key='topology'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .map(|r| serde_json::from_str::<ObjectRef>(&r))
        .transpose()?;
    if let Some(reference) = reference {
        let base: HardlinkTopology =
            serde_json::from_slice(&view.manager().store().read_object(&reference)?)?;
        for group in base.groups {
            let first = decode_path(
                group
                    .paths_hex
                    .first()
                    .context("empty inherited hardlink group")?,
            )?;
            let id = encode_path(&first);
            let mut known = c
                .query_row(
                    "SELECT inode FROM inherited_groups WHERE group_id=?",
                    [&id],
                    |r| read_id(r, 0),
                )
                .optional()?;
            if known.is_none() {
                for path in &group.paths_hex {
                    let _ = original_lookup(view, c, &decode_path(path)?)?;
                    known = c
                        .query_row(
                            "SELECT inode FROM inherited_groups WHERE group_id=?",
                            [&id],
                            |r| read_id(r, 0),
                        )
                        .optional()?;
                    if known.is_some() {
                        break;
                    }
                }
            }
            if let Some(inode) = known {
                if node(c, inode)?.attr.nlink > 0 {
                    hydrate_links(view, c, inode)?;
                }
            }
        }
    }
    // This query visits only hydrated/private linked inodes, never all inherited
    // independent files. Group membership comes from identity, never blob hash.
    let mut q = c.prepare("SELECT id FROM nodes WHERE kind=1 AND nlink>1 ORDER BY id")?;
    let ids = q
        .query_map([], |r| read_id(r, 0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(q);
    let mut groups = Vec::new();
    let mut total_paths = 0usize;
    for inode in ids {
        let mut q =
            c.prepare("SELECT parent,name FROM dirents WHERE child=? ORDER BY parent,name")?;
        let bindings = q
            .query_map([sql_id(inode)], |r| {
                Ok((read_id(r, 0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        let mut paths = Vec::new();
        for (parent, name) in bindings {
            if let Some(mut path) = directory_path(c, parent)? {
                path.push(name);
                paths.push(path);
            }
        }
        paths.sort();
        paths.dedup();
        ensure!(
            paths.len() as u64 == node(c, inode)?.attr.nlink,
            "hardlink identity/count differs from reachable bindings"
        );
        total_paths = total_paths
            .checked_add(paths.len())
            .context("topology path overflow")?;
        ensure!(
            total_paths <= 1_000_000 && groups.len() < 100_000,
            "topology exceeds supported path/group bound"
        );
        if paths.len() > 1 {
            groups.push(paths);
        }
    }
    groups.sort();
    let topology = HardlinkTopology {
        format_version: 1,
        groups: groups
            .into_iter()
            .map(|paths| HardlinkGroup {
                paths_hex: paths
                    .into_iter()
                    .map(|path| path.into_iter().map(hex::encode).collect())
                    .collect(),
            })
            .collect(),
    };
    ensure!(
        canonical_json(&topology)?.len() <= 64 * 1024 * 1024,
        "topology exceeds64MiB consumer bound"
    );
    Ok(topology)
}
