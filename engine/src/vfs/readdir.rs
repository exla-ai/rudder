//! Remote-only, fixed directory snapshots. Cookies and private overlay snapshots
//! are reconstructible and quota-limited; inode reservations live with the
//! authoritative namespace and are never swept/reused. Listing hints reserve
//! identity/type only, never attributes or file content.
use super::*;

pub const DIRECTORY_PAGE_ENTRIES: usize = 256;

#[derive(Debug, Clone)]
pub struct ReaddirLimits {
    pub max_live_cursors: u64,
    pub max_cookies_per_cursor: u64,
    pub max_total_cookies: u64,
    pub max_overlay_rows_per_cursor: u64,
    pub max_total_overlay_rows: u64,
    pub max_total_overlay_bytes: u64,
    /// Main cursor DB cap. DELETE rollback journal is separately bounded by
    /// at most this many touched pages plus its header; there is no WAL.
    pub max_cursor_db_bytes: u64,
    pub max_reserved_inodes: u64,
    pub max_reserved_bytes: u64,
}
impl Default for ReaddirLimits {
    fn default() -> Self {
        Self {
            max_live_cursors: 32,
            max_cookies_per_cursor: 131_072,
            max_total_cookies: 524_288,
            max_overlay_rows_per_cursor: 65_536,
            max_total_overlay_rows: 131_072,
            max_total_overlay_bytes: 64 * 1024 * 1024,
            max_cursor_db_bytes: 128 * 1024 * 1024,
            max_reserved_inodes: 524_288,
            max_reserved_bytes: 128 * 1024 * 1024,
        }
    }
}
impl ReaddirLimits {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.max_live_cursors > 0 && self.max_live_cursors <= 1024,
            "invalid readdir cursor count"
        );
        ensure!(
            self.max_cookies_per_cursor > 0
                && self.max_cookies_per_cursor <= 1_048_576
                && self.max_total_cookies >= self.max_cookies_per_cursor
                && self.max_total_cookies <= 4_194_304,
            "invalid readdir cookie quota"
        );
        ensure!(
            self.max_overlay_rows_per_cursor > 0
                && self.max_overlay_rows_per_cursor <= 1_048_576
                && self.max_total_overlay_rows >= self.max_overlay_rows_per_cursor
                && self.max_total_overlay_rows <= 4_194_304,
            "invalid readdir overlay quota"
        );
        ensure!(
            self.max_total_overlay_bytes >= 4096
                && self.max_total_overlay_bytes <= 512 * 1024 * 1024
                && self.max_cursor_db_bytes >= 64 * 1024
                && self.max_cursor_db_bytes <= 512 * 1024 * 1024,
            "invalid readdir storage quota"
        );
        ensure!(
            self.max_reserved_inodes > 0
                && self.max_reserved_inodes <= 4_194_304
                && self.max_reserved_bytes >= 4096
                && self.max_reserved_bytes <= 1024 * 1024 * 1024,
            "invalid readdir identity quota"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: Vec<u8>,
    pub ino: u64,
    pub kind: NodeKind,
    pub next_cookie: u64,
}
#[derive(Debug, Clone)]
pub struct DirectoryPage {
    pub entries: Vec<DirectoryEntry>,
    pub eof: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct ReaddirStats {
    pub pages_served: u64,
    pub namespace_identity_commits: u64,
    pub cursor_commits: u64,
    pub scan_ns: u64,
    pub identity_ns: u64,
    pub cookie_ns: u64,
    pub namespace_commit_ns: u64,
    pub namespace_checkpoint_ns: u64,
    pub cursor_commit_ns: u64,
    pub live_cursors: u64,
    pub cookies: u64,
    pub overlay_rows: u64,
    pub overlay_bytes: u64,
    pub cursor_db_bytes: u64,
    pub reserved_inodes: u64,
    pub reserved_bytes: u64,
}

pub(super) fn initialize(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE readdir_identities(origin TEXT PRIMARY KEY,inode INTEGER NOT NULL UNIQUE,kind INTEGER NOT NULL,parent INTEGER NOT NULL,name BLOB NOT NULL,cost INTEGER NOT NULL CHECK(cost>=0));CREATE TABLE readdir_identity_totals(singleton INTEGER PRIMARY KEY CHECK(singleton=1),entries INTEGER NOT NULL CHECK(entries>=0),bytes INTEGER NOT NULL CHECK(bytes>=0));INSERT INTO readdir_identity_totals VALUES(1,0,0);")?;
    Ok(())
}

/// Only used under the namespace connection/transaction lock. The allocator
/// shares SQLite's AUTOINCREMENT sequence with ordinary create; a page commits
/// counters once instead of re-reading/re-writing them for every inherited name.
struct IdentityBatch {
    initial: u64,
    last: u64,
    entries: u64,
    bytes: u64,
}
impl IdentityBatch {
    fn new(c: &Connection) -> Result<Self> {
        let initial = c.query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='nodes'",
            [],
            |r| read_id(r, 0),
        )?;
        let (entries, bytes) = c.query_row(
            "SELECT entries,bytes FROM readdir_identity_totals WHERE singleton=1",
            [],
            |r| Ok((read_id(r, 0)?, read_id(r, 1)?)),
        )?;
        Ok(Self {
            initial,
            last: initial,
            entries,
            bytes,
        })
    }
    fn reserve(
        &mut self,
        c: &Connection,
        origin: &str,
        parent: u64,
        name: &[u8],
        kind: NodeKind,
        limits: &ReaddirLimits,
    ) -> Result<u64> {
        name_ok(name)?;
        if let Some((id, actual_kind, actual_parent, actual_name)) = c
            .prepare_cached("SELECT inode,kind,parent,name FROM readdir_identities WHERE origin=?")?
            .query_row([origin], |r| {
                Ok((
                    read_id(r, 0)?,
                    r.get::<_, i64>(1)?,
                    read_id(r, 2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            })
            .optional()?
        {
            ensure!(
                actual_kind == kind.code() && actual_parent == parent && actual_name == name,
                "inherited reservation identity differs"
            );
            return Ok(id);
        }
        let cost = 128u64
            .checked_add(origin.len() as u64)
            .and_then(|n| n.checked_add(name.len() as u64))
            .context("inode reservation bytes overflow")?;
        let bytes = self
            .bytes
            .checked_add(cost)
            .filter(|v| *v <= limits.max_reserved_bytes)
            .ok_or_else(|| error(libc::EAGAIN))?;
        if self.entries >= limits.max_reserved_inodes {
            return Err(error(libc::EAGAIN));
        }
        let id = self
            .last
            .checked_add(1)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or_else(|| error(libc::ENOSPC))?;
        c.prepare_cached("INSERT INTO readdir_identities VALUES(?,?,?,?,?,?)")?
            .execute(params![
                origin,
                sql_id(id),
                kind.code(),
                sql_id(parent),
                name,
                sql_id(cost)
            ])?;
        self.last = id;
        self.entries += 1;
        self.bytes = bytes;
        Ok(id)
    }
    fn finish(self, c: &Connection) -> Result<()> {
        if self.last != self.initial {
            ensure!(
                c.execute(
                    "UPDATE sqlite_sequence SET seq=? WHERE name='nodes' AND seq=?",
                    params![sql_id(self.last), sql_id(self.initial)]
                )? == 1,
                "inherited allocator changed within transaction"
            );
            c.execute(
                "UPDATE readdir_identity_totals SET entries=?,bytes=? WHERE singleton=1",
                params![sql_id(self.entries), sql_id(self.bytes)],
            )?;
        }
        Ok(())
    }
}
/// Inherited identities are origin-based, never content-hash based. This
/// single-entry form is for ordinary lookup/getattr, not the hot listing loop.
pub(super) fn reserve(
    c: &Connection,
    origin: &str,
    parent: u64,
    name: &[u8],
    kind: NodeKind,
    limits: &ReaddirLimits,
) -> Result<u64> {
    let mut batch = IdentityBatch::new(c)?;
    let id = batch.reserve(c, origin, parent, name, kind, limits)?;
    batch.finish(c)?;
    Ok(id)
}

pub(super) fn reserved(
    c: &Connection,
    ino: u64,
) -> Result<Option<(u64, Vec<u8>, String, NodeKind)>> {
    let row = c
        .query_row(
            "SELECT parent,name,origin,kind FROM readdir_identities WHERE inode=?",
            [sql_id(ino)],
            |r| {
                Ok((
                    read_id(r, 0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(p, n, o, k)| Ok((p, n, o, NodeKind::from_code(k)?)))
        .transpose()
}

#[derive(Default)]
struct Metrics {
    pages: u64,
    identity_commits: u64,
    cursor_commits: u64,
    scan: u64,
    identity: u64,
    cookie: u64,
    namespace_commit: u64,
    namespace_checkpoint: u64,
    cursor_commit: u64,
}
fn elapsed_ns(start: std::time::Instant) -> u64 {
    start.elapsed().as_nanos().min(u64::MAX as u128) as u64
}
pub(super) struct Pager {
    connection: Connection,
    limits: ReaddirLimits,
    metrics: Metrics,
}
#[derive(Clone)]
struct Snapshot {
    ino: u64,
    parent: u64,
    base: Option<ObjectRef>,
    origin: Option<String>,
    count: u64,
    next: u64,
    cookies: u64,
}
#[derive(Clone)]
struct Hint {
    name: Vec<u8>,
    ino: Option<u64>,
    kind: Option<NodeKind>,
}
impl Pager {
    pub(super) fn open(path: &Path, limits: ReaddirLimits) -> Result<Self> {
        limits.validate()?;
        // The per-view owner lock is already held. Only these reconstructible
        // files are reset; namespace/private backing files are never removed.
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{suffix}", path.display()));
            match fs::symlink_metadata(&p) {
                Ok(m) => {
                    ensure!(
                        m.is_file() && !m.file_type().is_symlink(),
                        "unsafe cursor database path"
                    );
                    fs::remove_file(&p)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.execute_batch("PRAGMA page_size=4096;PRAGMA journal_mode=DELETE;PRAGMA synchronous=OFF;PRAGMA foreign_keys=ON;PRAGMA temp_store=MEMORY;")?;
        let pages = limits.max_cursor_db_bytes / 4096;
        connection.pragma_update(None, "max_page_count", sql_id(pages))?;
        connection.pragma_update(None, "journal_size_limit", 0)?;
        connection.execute_batch("CREATE TABLE cursors(id INTEGER PRIMARY KEY,inode INTEGER NOT NULL,parent INTEGER NOT NULL,base TEXT,origin TEXT,entry_count INTEGER NOT NULL,next_cookie INTEGER NOT NULL,cookie_count INTEGER NOT NULL DEFAULT 0,overlay_rows INTEGER NOT NULL,overlay_bytes INTEGER NOT NULL);CREATE TABLE overlay(cursor INTEGER NOT NULL REFERENCES cursors(id) ON DELETE CASCADE,name BLOB NOT NULL,inode INTEGER,kind INTEGER,PRIMARY KEY(cursor,name)) WITHOUT ROWID;CREATE TABLE cookies(cursor INTEGER NOT NULL REFERENCES cursors(id) ON DELETE CASCADE,cookie INTEGER NOT NULL,name BLOB NOT NULL,PRIMARY KEY(cursor,cookie),UNIQUE(cursor,name)) WITHOUT ROWID;CREATE TABLE totals(singleton INTEGER PRIMARY KEY CHECK(singleton=1),cookies INTEGER NOT NULL,overlay_rows INTEGER NOT NULL,overlay_bytes INTEGER NOT NULL);INSERT INTO totals VALUES(1,0,0,0);")?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        File::open(path.parent().context("cursor parent")?)?.sync_all()?;
        Ok(Self {
            connection,
            limits,
            metrics: Metrics::default(),
        })
    }
    fn snapshot(
        cp: &Connection,
        ns: &Connection,
        handle: u64,
        ino: u64,
        limits: &ReaddirLimits,
    ) -> Result<Snapshot> {
        let existing=cp.query_row("SELECT inode,parent,base,origin,entry_count,next_cookie,cookie_count FROM cursors WHERE id=?",[sql_id(handle)],|r|Ok((read_id(r,0)?,read_id(r,1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,read_id(r,4)?,read_id(r,5)?,read_id(r,6)?))).optional()?;
        if let Some((actual, parent, base, origin, count, next, cookies)) = existing {
            ensure!(actual == ino, "directory cursor inode differs");
            return Ok(Snapshot {
                ino,
                parent,
                base: base.map(|s| serde_json::from_str(&s)).transpose()?,
                origin,
                count,
                next,
                cookies,
            });
        }
        let live = cp.query_row("SELECT count(*) FROM cursors", [], |r| read_id(r, 0))?;
        if live >= limits.max_live_cursors {
            return Err(error(libc::EAGAIN));
        }
        let n = require_dir(ns, ino)?;
        let count = ns.query_row(
            "SELECT entry_count FROM directory_sizes WHERE inode=?",
            [sql_id(ino)],
            |r| read_id(r, 0),
        )?;
        // Only private mutation witnesses are copied, never the inherited lookup
        // cache or a scan of the immutable base directory.
        let (rows,bytes)=ns.query_row("SELECT count(*),COALESCE(sum(length(d.name)+64),0) FROM dirty_dirents d JOIN dirents e ON d.parent=e.parent AND d.name=e.name WHERE d.parent=?",[sql_id(ino)],|r|Ok((read_id(r,0)?,read_id(r,1)?)))?;
        let (total_rows, total_bytes) = cp.query_row(
            "SELECT overlay_rows,overlay_bytes FROM totals WHERE singleton=1",
            [],
            |r| Ok((read_id(r, 0)?, read_id(r, 1)?)),
        )?;
        if rows > limits.max_overlay_rows_per_cursor
            || total_rows
                .checked_add(rows)
                .is_none_or(|v| v > limits.max_total_overlay_rows)
            || total_bytes
                .checked_add(bytes)
                .is_none_or(|v| v > limits.max_total_overlay_bytes)
        {
            return Err(error(libc::EAGAIN));
        }
        cp.execute(
            "INSERT INTO cursors VALUES(?,?,?,?,?,?,3,0,?,?)",
            params![
                sql_id(handle),
                sql_id(ino),
                sql_id(n.attr.parent),
                n.base.as_ref().map(json_ref).transpose()?,
                n.origin,
                sql_id(count),
                sql_id(rows),
                sql_id(bytes)
            ],
        )?;
        let mut q=ns.prepare("SELECT d.name,e.child,n.kind FROM dirty_dirents d JOIN dirents e ON d.parent=e.parent AND d.name=e.name LEFT JOIN nodes n ON e.child=n.id WHERE d.parent=? ORDER BY d.name")?;
        for row in q.query_map([sql_id(ino)], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })? {
            let (name, child, kind) = row?;
            ensure!(
                child.is_none() == kind.is_none(),
                "private directory child is missing"
            );
            cp.execute(
                "INSERT INTO overlay VALUES(?,?,?,?)",
                params![sql_id(handle), name, child, kind],
            )?;
        }
        cp.execute("UPDATE totals SET overlay_rows=overlay_rows+?,overlay_bytes=overlay_bytes+? WHERE singleton=1",params![sql_id(rows),sql_id(bytes)])?;
        Ok(Snapshot {
            ino,
            parent: n.attr.parent,
            base: n.base,
            origin: n.origin,
            count,
            next: 3,
            cookies: 0,
        })
    }
    pub(super) fn page(
        &mut self,
        view: &VfsView,
        ns: &mut Connection,
        handle: u64,
        ino: u64,
        cookie: u64,
        limit: usize,
    ) -> Result<DirectoryPage> {
        ensure!(
            limit > 0 && limit <= DIRECTORY_PAGE_ENTRIES,
            "invalid directory page size"
        );
        let result = (|| -> Result<DirectoryPage> {
            let nt = ns.transaction()?;
            let ct = self.connection.transaction()?;
            let mut snapshot = Self::snapshot(&ct, &nt, handle, ino, &self.limits)?;
            let mut identities = IdentityBatch::new(&nt)?;
            let mut total_cookies =
                ct.query_row("SELECT cookies FROM totals WHERE singleton=1", [], |r| {
                    read_id(r, 0)
                })?;
            let mut after = if cookie <= 2 {
                None
            } else {
                Some(
                    ct.query_row(
                        "SELECT name FROM cookies WHERE cursor=? AND cookie=?",
                        params![sql_id(handle), sql_id(cookie)],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .optional()?
                    .ok_or_else(|| error(libc::EINVAL))?,
                )
            };
            let mut output = Vec::with_capacity(limit);
            if cookie == 0 {
                output.push(DirectoryEntry {
                    name: b".".to_vec(),
                    ino,
                    kind: NodeKind::Directory,
                    next_cookie: 1,
                });
            }
            if cookie <= 1 && output.len() < limit {
                output.push(DirectoryEntry {
                    name: b"..".to_vec(),
                    ino: snapshot.parent,
                    kind: NodeKind::Directory,
                    next_cookie: 2,
                });
            }
            let mut eof = snapshot.base.is_none()
                && snapshot.count == 0
                && !(cookie == 0 && output.len() == 1);
            let remote = view
                .manager()
                .0
                .remote
                .as_ref()
                .context("remote cursor on local view")?;
            let deadline = remote.page_deadline();
            let mut scanned = 0usize;
            // Bounded work as well as resident buffers. An exhausted metadata
            // budget returns an explicit error, never a false clean EOF.
            let scan_limit = (self.limits.max_overlay_rows_per_cursor as usize)
                .saturating_add(DIRECTORY_PAGE_ENTRIES * 2);
            while output.len() < limit && !eof {
                if std::time::Instant::now() >= deadline {
                    return Err(error(libc::ETIMEDOUT));
                }
                let scan_started = std::time::Instant::now();
                let inherited = if let Some(base) = &snapshot.base {
                    remote.scan(
                        snapshot.origin.as_deref(),
                        base,
                        after.as_deref(),
                        DIRECTORY_PAGE_ENTRIES,
                    )?
                } else {
                    remote::NamePage {
                        entries: Vec::new(),
                        eof: true,
                    }
                };
                self.metrics.scan = self.metrics.scan.saturating_add(elapsed_ns(scan_started));
                let mut q=ct.prepare("SELECT name,inode,kind FROM overlay WHERE cursor=? AND name>? ORDER BY name LIMIT ?")?;
                let overlays = q
                    .query_map(
                        params![
                            sql_id(handle),
                            after.as_deref().unwrap_or(b""),
                            (DIRECTORY_PAGE_ENTRIES + 1) as i64
                        ],
                        |r| {
                            Ok((
                                r.get::<_, Vec<u8>>(0)?,
                                r.get::<_, Option<i64>>(1)?,
                                r.get::<_, Option<i64>>(2)?,
                            ))
                        },
                    )?
                    .map(|r| {
                        let (n, id, k) = r?;
                        Ok(Hint {
                            name: n,
                            ino: id.map(|v| v as u64),
                            kind: k.map(NodeKind::from_code).transpose()?,
                        })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(q);
                let overlays_more = overlays.len() > DIRECTORY_PAGE_ENTRIES;
                let overlays = &overlays[..overlays.len().min(DIRECTORY_PAGE_ENTRIES)];
                let mut base_rows = Vec::with_capacity(inherited.entries.len());
                for entry in inherited.entries {
                    let name = entry.name()?;
                    let kind = match entry.node {
                        crate::directory::EntryNode::File { .. } => NodeKind::File,
                        crate::directory::EntryNode::Directory { .. } => NodeKind::Directory,
                        crate::directory::EntryNode::Symlink { .. } => NodeKind::Symlink,
                    };
                    base_rows.push(Hint {
                        name,
                        ino: None,
                        kind: Some(kind),
                    });
                }
                let mut b = 0;
                let mut p = 0;
                let mut advanced = false;
                while output.len() < limit {
                    let base = base_rows.get(b);
                    let private = overlays.get(p);
                    // Exhaustion of a non-final input page requires the next
                    // page before selecting a potentially later other input.
                    if base.is_none() && !inherited.eof || private.is_none() && overlays_more {
                        break;
                    }
                    let (hint, is_private) = match (base, private) {
                        (None, None) => break,
                        (Some(v), None) => {
                            b += 1;
                            (v, false)
                        }
                        (None, Some(v)) => {
                            p += 1;
                            (v, true)
                        }
                        (Some(left), Some(right)) => match left.name.cmp(&right.name) {
                            std::cmp::Ordering::Less => {
                                b += 1;
                                (left, false)
                            }
                            std::cmp::Ordering::Equal => {
                                b += 1;
                                p += 1;
                                (right, true)
                            }
                            std::cmp::Ordering::Greater => {
                                p += 1;
                                (right, true)
                            }
                        },
                    };
                    advanced = true;
                    scanned += 1;
                    if scanned > scan_limit {
                        return Err(error(libc::EAGAIN));
                    }
                    after = Some(hint.name.clone());
                    let Some(kind) = hint.kind else {
                        continue;
                    };
                    let identity_started = std::time::Instant::now();
                    let id = if is_private {
                        hint.ino.context("private snapshot child missing")?
                    } else {
                        let origin = if snapshot.origin.as_deref() == Some("") {
                            hex::encode(&hint.name)
                        } else {
                            format!(
                                "{}/{}",
                                snapshot
                                    .origin
                                    .as_deref()
                                    .context("inherited snapshot origin missing")?,
                                hex::encode(&hint.name)
                            )
                        };
                        remote.check_origin(&origin)?;
                        identities.reserve(
                            &nt,
                            &origin,
                            snapshot.ino,
                            &hint.name,
                            kind,
                            &self.limits,
                        )?
                    };
                    self.metrics.identity = self
                        .metrics
                        .identity
                        .saturating_add(elapsed_ns(identity_started));
                    let cookie_started = std::time::Instant::now();
                    let existing = ct
                        .prepare_cached("SELECT cookie FROM cookies WHERE cursor=? AND name=?")?
                        .query_row(params![sql_id(handle), hint.name], |r| read_id(r, 0))
                        .optional()?;
                    let next_cookie = if let Some(v) = existing {
                        v
                    } else {
                        if snapshot.cookies >= self.limits.max_cookies_per_cursor
                            || total_cookies >= self.limits.max_total_cookies
                        {
                            return Err(error(libc::EAGAIN));
                        }
                        let v = snapshot.next;
                        snapshot.next = snapshot
                            .next
                            .checked_add(1)
                            .filter(|n| *n <= i64::MAX as u64)
                            .ok_or_else(|| error(libc::EOVERFLOW))?;
                        ct.prepare_cached("INSERT INTO cookies VALUES(?,?,?)")?
                            .execute(params![sql_id(handle), sql_id(v), hint.name])?;
                        total_cookies += 1;
                        snapshot.cookies += 1;
                        v
                    };
                    self.metrics.cookie = self
                        .metrics
                        .cookie
                        .saturating_add(elapsed_ns(cookie_started));
                    output.push(DirectoryEntry {
                        name: hint.name.clone(),
                        ino: id,
                        kind,
                        next_cookie,
                    });
                }
                eof =
                    b == base_rows.len() && inherited.eof && p == overlays.len() && !overlays_more;
                if !eof && !advanced {
                    return Err(error(libc::EIO)).context("directory merge made no progress");
                }
            }
            ct.execute(
                "UPDATE cursors SET next_cookie=?,cookie_count=? WHERE id=?",
                params![
                    sql_id(snapshot.next),
                    sql_id(snapshot.cookies),
                    sql_id(handle)
                ],
            )?;
            ct.execute(
                "UPDATE totals SET cookies=? WHERE singleton=1",
                [sql_id(total_cookies)],
            )?;
            if std::time::Instant::now() >= deadline {
                return Err(error(libc::ETIMEDOUT));
            }
            let new_identities = identities.last != identities.initial;
            identities.finish(&nt)?;
            remote.guard()?;
            // There is no cross-DB atomicity claim. A cursor commit failure can
            // leave unused, bounded identities, never dangling cookie identity.
            let commit_started = std::time::Instant::now();
            nt.commit()?;
            self.metrics.namespace_commit = self
                .metrics
                .namespace_commit
                .saturating_add(elapsed_ns(commit_started));
            if new_identities {
                self.metrics.identity_commits = self.metrics.identity_commits.saturating_add(1);
            }
            // This connection owns the namespace lock; do not let listing-only
            // reservation batches accumulate an unbounded namespace WAL.
            if new_identities {
                let checkpoint_started = std::time::Instant::now();
                let busy = ns.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    r.get::<_, i64>(0)
                })?;
                self.metrics.namespace_checkpoint = self
                    .metrics
                    .namespace_checkpoint
                    .saturating_add(elapsed_ns(checkpoint_started));
                if busy != 0 {
                    return Err(error(libc::EAGAIN));
                }
            }
            let commit_started = std::time::Instant::now();
            ct.commit()?;
            self.metrics.cursor_commit = self
                .metrics
                .cursor_commit
                .saturating_add(elapsed_ns(commit_started));
            self.metrics.cursor_commits = self.metrics.cursor_commits.saturating_add(1);
            remote.guard()?;
            self.metrics.pages = self.metrics.pages.saturating_add(1);
            Ok(DirectoryPage {
                entries: output,
                eof,
            })
        })();
        result.map_err(|e|if e.downcast_ref::<rusqlite::Error>().is_some_and(|e|matches!(e,rusqlite::Error::SqliteFailure(code,_)if code.code==rusqlite::ErrorCode::DiskFull)){error(libc::EAGAIN)}else{e})
    }
    pub(super) fn close(&mut self, handle: u64) -> Result<()> {
        let tx = self.connection.transaction()?;
        if let Some((cookies, rows, bytes)) = tx
            .query_row(
                "SELECT cookie_count,overlay_rows,overlay_bytes FROM cursors WHERE id=?",
                [sql_id(handle)],
                |r| Ok((read_id(r, 0)?, read_id(r, 1)?, read_id(r, 2)?)),
            )
            .optional()?
        {
            tx.execute("UPDATE totals SET cookies=cookies-?,overlay_rows=overlay_rows-?,overlay_bytes=overlay_bytes-? WHERE singleton=1",params![sql_id(cookies),sql_id(rows),sql_id(bytes)])?;
            tx.execute("DELETE FROM cursors WHERE id=?", [sql_id(handle)])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(super) fn stats(&self, ns: &Connection) -> Result<ReaddirStats> {
        let (cookies, overlay_rows, overlay_bytes) = self.connection.query_row(
            "SELECT cookies,overlay_rows,overlay_bytes FROM totals WHERE singleton=1",
            [],
            |r| Ok((read_id(r, 0)?, read_id(r, 1)?, read_id(r, 2)?)),
        )?;
        let (reserved_inodes, reserved_bytes) = ns.query_row(
            "SELECT entries,bytes FROM readdir_identity_totals WHERE singleton=1",
            [],
            |r| Ok((read_id(r, 0)?, read_id(r, 1)?)),
        )?;
        let pages = self
            .connection
            .query_row("PRAGMA page_count", [], |r| read_id(r, 0))?;
        Ok(ReaddirStats {
            pages_served: self.metrics.pages,
            namespace_identity_commits: self.metrics.identity_commits,
            cursor_commits: self.metrics.cursor_commits,
            scan_ns: self.metrics.scan,
            identity_ns: self.metrics.identity,
            cookie_ns: self.metrics.cookie,
            namespace_commit_ns: self.metrics.namespace_commit,
            namespace_checkpoint_ns: self.metrics.namespace_checkpoint,
            cursor_commit_ns: self.metrics.cursor_commit,
            live_cursors: self
                .connection
                .query_row("SELECT count(*) FROM cursors", [], |r| read_id(r, 0))?,
            cookies,
            overlay_rows,
            overlay_bytes,
            cursor_db_bytes: pages * 4096,
            reserved_inodes,
            reserved_bytes,
        })
    }
}
