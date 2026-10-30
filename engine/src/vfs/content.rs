//! Private range CoW for the explicit VFS v2 profile. Data records are immutable
//! 64 KiB blocks in a per-inode append-only file. A FULL SQLite transaction may
//! publish a pointer only after the corresponding log has been synced. Failed
//! writes leave unreferenced tails, never a pointer to an undurable only copy.
//! File length and the truncation fence are metadata, not the log's byte length.

use super::observation::WorkMeter;
use super::*;
use crate::content::{self as cas, ContentStore, FileNode, StagedContentStore};

pub const PRIVATE_BLOCK_BYTES: usize = 64 * 1024;
const BLOCK: u64 = PRIVATE_BLOCK_BYTES as u64;

pub(super) fn initialize(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE file_overlays(inode INTEGER PRIMARY KEY,base_limit INTEGER NOT NULL CHECK(base_limit>=0));CREATE TABLE file_blocks(inode INTEGER NOT NULL,block_index INTEGER NOT NULL,log_offset INTEGER NOT NULL,length INTEGER NOT NULL,checksum TEXT NOT NULL,PRIMARY KEY(inode,block_index)) WITHOUT ROWID;")?;
    Ok(())
}

/// The cache holds verified immutable Blob FDs, not descriptor FDs or data Vecs.
pub(super) struct Reader<'a>(pub &'a VfsView, pub Option<&'a WorkMeter<'a>>);
impl ContentStore for Reader<'_> {
    fn put_blob(&self, _: &[u8]) -> Result<ObjectRef> {
        bail!("read-only VFS content resolver")
    }
    fn put_file_node(&self, _: &FileNode) -> Result<ObjectRef> {
        bail!("read-only VFS content resolver")
    }
    fn read_object(&self, r: &ObjectRef) -> Result<Vec<u8>> {
        self.0.require_local_checkpoint()?;
        if let Some(meter) = self.1 {
            meter.metadata(r.size_bytes)?;
        }
        self.0.manager().store().read_object(r)
    }
    fn read_blob_range(&self, r: &ObjectRef, offset: u64, length: usize) -> Result<Vec<u8>> {
        ensure!(
            r.kind == ObjectKind::Blob && length <= MAX_IO,
            "invalid bounded VFS blob read"
        );
        let count = r.size_bytes.saturating_sub(offset).min(length as u64) as usize;
        if count == 0 {
            return Ok(Vec::new());
        }
        if let Some(meter) = self.1 {
            meter.blob(r)?;
        }
        let f = self.0.inherited_file(r)?;
        let mut bytes = vec![0; count];
        read_exact_at(&f, &mut bytes, offset)?;
        Ok(bytes)
    }
    fn copy_blob_to(&self, r: &ObjectRef, writer: &mut dyn Write) -> Result<()> {
        ensure!(r.kind == ObjectKind::Blob, "expected raw blob");
        if let Some(meter) = self.1 {
            meter.blob(r)?;
        }
        let f = self.0.inherited_file(r)?;
        let mut buffer = vec![0; PRIVATE_BLOCK_BYTES];
        let mut offset = 0;
        while offset < r.size_bytes {
            let count = (r.size_bytes - offset).min(BLOCK) as usize;
            read_exact_at(&f, &mut buffer[..count], offset)?;
            writer.write_all(&buffer[..count])?;
            offset += count as u64;
        }
        Ok(())
    }
}
fn read_exact_at(f: &File, bytes: &mut [u8], offset: u64) -> Result<()> {
    let mut done = 0;
    while done < bytes.len() {
        match f.read_at(
            &mut bytes[done..],
            offset
                .checked_add(done as u64)
                .context("read offset overflow")?,
        ) {
            Ok(0) => bail!("private or immutable data ended before its acknowledged length"),
            Ok(count) => done += count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn base_limit(c: &Connection, n: &Node) -> Result<u64> {
    Ok(c.query_row(
        "SELECT base_limit FROM file_overlays WHERE inode=?",
        [sql_id(n.attr.ino)],
        |r| read_id(r, 0),
    )
    .optional()?
    .unwrap_or(n.attr.size))
}
fn initialize_inode(c: &Connection, n: &Node) -> Result<()> {
    ensure!(
        !c.is_autocommit(),
        "range mutation requires the caller's durable transaction"
    );
    c.execute(
        "INSERT OR IGNORE INTO file_overlays VALUES(?,?)",
        params![sql_id(n.attr.ino), sql_id(n.attr.size)],
    )?;
    Ok(())
}
#[derive(Debug)]
struct Pointer {
    offset: u64,
    length: u64,
    checksum: String,
}
fn pointer(c: &Connection, inode: u64, index: u64) -> Result<Option<Pointer>> {
    Ok(c.query_row(
        "SELECT log_offset,length,checksum FROM file_blocks WHERE inode=? AND block_index=?",
        params![sql_id(inode), sql_id(index)],
        |r| {
            Ok(Pointer {
                offset: read_id(r, 0)?,
                length: read_id(r, 1)?,
                checksum: r.get(2)?,
            })
        },
    )
    .optional()?)
}
fn validate_pointer(f: &File, p: &Pointer) -> Result<()> {
    ensure!(
        p.length == BLOCK && p.offset % BLOCK == 0,
        "invalid private block record bounds"
    );
    let r = ObjectRef {
        kind: ObjectKind::Blob,
        algorithm: crate::protocol::HashAlgorithm::Sha256,
        digest: p.checksum.clone(),
        size_bytes: p.length,
    };
    r.validate()?;
    ensure!(
        p.offset
            .checked_add(p.length)
            .is_some_and(|end| end <= f.metadata().map(|m| m.len()).unwrap_or(0)),
        "private block pointer exceeds its durable log"
    );
    Ok(())
}
fn private_block(
    view: &VfsView,
    c: &Connection,
    n: &Node,
    index: u64,
    meter: Option<&WorkMeter<'_>>,
) -> Result<Option<Vec<u8>>> {
    let Some(p) = pointer(c, n.attr.ino, index)? else {
        return Ok(None);
    };
    if let Some(meter) = meter {
        meter.content(BLOCK)?;
    }
    let f = view.private_file(
        n.backing
            .as_ref()
            .context("private blocks lack their log")?,
        false,
    )?;
    validate_pointer(&f, &p)?;
    let mut bytes = vec![0; PRIVATE_BLOCK_BYTES];
    read_exact_at(&f, &mut bytes, p.offset)?;
    ensure!(
        hex::encode(Sha256::digest(&bytes)) == p.checksum,
        "private block checksum mismatch"
    );
    Ok(Some(bytes))
}
fn base_range(
    view: &VfsView,
    n: &Node,
    limit: u64,
    offset: u64,
    length: usize,
    meter: Option<&WorkMeter<'_>>,
) -> Result<Vec<u8>> {
    let count = limit.saturating_sub(offset).min(length as u64) as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    let object = n.base.as_ref().context("inherited range lacks content")?;
    if let Some(remote) = &view.manager().0.remote {
        if let Some(meter) = meter {
            meter.content(count as u64)?;
        }
        let bytes = remote.read_range(n.origin.as_deref(), object, offset, count)?;
        ensure!(
            bytes.len() == count,
            "remote inherited content shorter than acknowledged base"
        );
        return Ok(bytes);
    }
    let bytes = cas::read_file_range(&Reader(view, meter), object, offset, count)?;
    ensure!(
        bytes.len() == count,
        "inherited content shorter than its acknowledged logical length"
    );
    Ok(bytes)
}
pub(super) fn read(
    view: &VfsView,
    c: &Connection,
    n: &Node,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    read_metered(view, c, n, offset, length, None)
}
fn read_metered(
    view: &VfsView,
    c: &Connection,
    n: &Node,
    offset: u64,
    length: usize,
    meter: Option<&WorkMeter<'_>>,
) -> Result<Vec<u8>> {
    ensure!(
        n.attr.kind == NodeKind::File && length <= MAX_IO,
        "invalid range read"
    );
    offset
        .checked_add(length as u64)
        .context("range read overflow")?;
    let count = n.attr.size.saturating_sub(offset).min(length as u64) as usize;
    let mut output = vec![0; count];
    let limit = base_limit(c, n)?;
    let mut done = 0;
    while done < count {
        let position = offset + done as u64;
        let index = position / BLOCK;
        let within = (position % BLOCK) as usize;
        let take = (PRIVATE_BLOCK_BYTES - within).min(count - done);
        if let Some(bytes) = private_block(view, c, n, index, meter)? {
            output[done..done + take].copy_from_slice(&bytes[within..within + take]);
        } else {
            let bytes = base_range(view, n, limit, position, take, meter)?;
            output[done..done + bytes.len()].copy_from_slice(&bytes);
        }
        done += take;
    }
    Ok(output)
}
fn log(view: &VfsView, c: &Connection, n: &Node) -> Result<(String, File)> {
    if let Some(id) = &n.backing {
        return Ok((id.clone(), view.private_file(id, true)?));
    }
    let id = view.new_backing(None)?; // empty durable log; never hydrates the base
    c.execute(
        "UPDATE nodes SET backing=? WHERE id=?",
        params![id, sql_id(n.attr.ino)],
    )?;
    Ok((id.clone(), view.private_file(&id, true)?))
}
fn append(c: &Connection, inode: u64, index: u64, f: &File, bytes: &[u8]) -> Result<()> {
    ensure!(
        !c.is_autocommit() && bytes.len() == PRIVATE_BLOCK_BYTES,
        "invalid private block append"
    );
    let end = f.metadata()?.len();
    // Never overwrite an orphan tail from an interrupted earlier append.
    let offset = end.checked_add(BLOCK - 1).context("private log overflow")? / BLOCK * BLOCK;
    ensure!(
        offset
            .checked_add(BLOCK)
            .is_some_and(|v| v <= i64::MAX as u64),
        "private log exceeds supported offset"
    );
    f.write_all_at(bytes, offset)?;
    c.execute(
        "INSERT OR REPLACE INTO file_blocks VALUES(?,?,?,?,?)",
        params![
            sql_id(inode),
            sql_id(index),
            sql_id(offset),
            PRIVATE_BLOCK_BYTES as i64,
            hex::encode(Sha256::digest(bytes))
        ],
    )?;
    Ok(())
}
pub(super) fn write(
    view: &VfsView,
    c: &Connection,
    n: &Node,
    offset: u64,
    bytes: &[u8],
) -> Result<()> {
    ensure!(
        n.attr.kind == NodeKind::File && bytes.len() <= MAX_IO,
        "invalid private range write"
    );
    let end = offset
        .checked_add(bytes.len() as u64)
        .context("file write overflow")?;
    ensure!(
        end <= i64::MAX as u64,
        "file write exceeds logical size limit"
    );
    if bytes.is_empty() {
        return Ok(());
    }
    initialize_inode(c, n)?;
    let limit = base_limit(c, n)?;
    let (id, f) = log(view, c, n)?;
    let mut current = n.clone();
    current.backing = Some(id);
    let mut done = 0;
    while done < bytes.len() {
        let position = offset + done as u64;
        let index = position / BLOCK;
        let within = (position % BLOCK) as usize;
        let take = (PRIVATE_BLOCK_BYTES - within).min(bytes.len() - done);
        let mut block = if let Some(block) = private_block(view, c, &current, index, None)? {
            block
        } else {
            let mut block = vec![0; PRIVATE_BLOCK_BYTES];
            let inherited = base_range(view, n, limit, index * BLOCK, PRIVATE_BLOCK_BYTES, None)?;
            block[..inherited.len()].copy_from_slice(&inherited);
            block
        };
        block[within..within + take].copy_from_slice(&bytes[done..done + take]);
        append(c, n.attr.ino, index, &f, &block)?;
        done += take;
    }
    // This precedes the outer FULL SQLite commit, even for ordinary write().
    // Only the explicit v2 profile advertises this stronger local guarantee.
    f.sync_data()?;
    c.execute(
        "UPDATE nodes SET size=? WHERE id=?",
        params![sql_id(n.attr.size.max(end)), sql_id(n.attr.ino)],
    )?;
    Ok(())
}
pub(super) fn truncate(view: &VfsView, c: &Connection, n: &Node, size: u64) -> Result<()> {
    ensure!(
        n.attr.kind == NodeKind::File && size <= i64::MAX as u64,
        "invalid range truncate"
    );
    initialize_inode(c, n)?;
    if size < n.attr.size {
        let limit = base_limit(c, n)?.min(size);
        c.execute(
            "UPDATE file_overlays SET base_limit=? WHERE inode=?",
            params![sql_id(limit), sql_id(n.attr.ino)],
        )?;
        let first_removed = size.checked_add(BLOCK - 1).context("truncate overflow")? / BLOCK;
        c.execute(
            "DELETE FROM file_blocks WHERE inode=? AND block_index>=?",
            params![sql_id(n.attr.ino), sql_id(first_removed)],
        )?;
        if size % BLOCK != 0 {
            let index = size / BLOCK;
            if let Some(mut block) = private_block(view, c, n, index, None)? {
                block[(size % BLOCK) as usize..].fill(0);
                let (_, f) = log(view, c, n)?;
                append(c, n.attr.ino, index, &f, &block)?;
                f.sync_data()?;
            }
        }
    }
    c.execute(
        "UPDATE nodes SET size=? WHERE id=?",
        params![sql_id(size), sql_id(n.attr.ino)],
    )?;
    Ok(())
}
pub(super) fn validate_logs(view: &VfsView, c: &Connection) -> Result<()> {
    let mut q=c.prepare("SELECT b.inode,b.log_offset,b.length,b.checksum,n.backing,n.kind FROM file_blocks b LEFT JOIN nodes n ON n.id=b.inode")?;
    let rows = q.query_map([], |r| {
        Ok((
            read_id(r, 0)?,
            Pointer {
                offset: read_id(r, 1)?,
                length: read_id(r, 2)?,
                checksum: r.get(3)?,
            },
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<i64>>(5)?,
        ))
    })?;
    for row in rows {
        let (_, pointer, backing, kind) = row?;
        ensure!(
            kind == Some(1),
            "private block references a missing/non-file inode"
        );
        let f = view.private_file(backing.as_ref().context("private block lacks log")?, false)?;
        validate_pointer(&f, &pointer)?;
    }
    Ok(())
}

/// Sealing only rereads dirty 4 MiB chunks; unchanged aligned File subtrees are
/// retained verbatim. A legacy large Blob has no chunk refs and must be streamed
/// into the new representation once. Sparse zero subtrees are shared by size.
pub(super) fn seal_metered(
    view: &VfsView,
    c: &Connection,
    n: &Node,
    staging: &str,
    meter: Option<&WorkMeter<'_>>,
) -> Result<ObjectRef> {
    seal_collected(view, c, n, staging, meter, None)
}
pub(super) fn seal_collected<'a>(
    view: &'a VfsView,
    c: &'a Connection,
    n: &'a Node,
    staging: &'a str,
    meter: Option<&'a WorkMeter<'a>>,
    collector: Option<&'a capture::Collector<'a>>,
) -> Result<ObjectRef> {
    if let Some(collector) = collector {
        collector.bind(n)?;
    }
    let dirty: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM file_blocks WHERE inode=?)",
        [sql_id(n.attr.ino)],
        |r| r.get(0),
    )?;
    let limit = base_limit(c, n)?;
    if !dirty && limit == n.attr.size {
        if let Some(base) = &n.base {
            let info = match collector {
                Some(collector) => cas::file_info(collector, base)?,
                None => cas::file_info(&Reader(view, meter), base)?,
            };
            if info.logical_size == n.attr.size {
                return Ok(base.clone());
            }
        }
    }
    let store = StagedContentStore::new(view.manager().store(), staging)?;
    let mut builder = Sealer {
        view,
        c,
        n,
        limit,
        store,
        meter,
        collector,
        metadata: BTreeMap::new(),
        metadata_order: VecDeque::new(),
        zeros: BTreeMap::new(),
    };
    builder.build(0, n.attr.size, n.attr.size > cas::CHUNK_BYTES as u64)
}
fn capacity(level: u8) -> u64 {
    (cas::CHUNK_BYTES as u128 * (cas::FANOUT as u128).pow(u32::from(level) + 1))
        .min(i64::MAX as u128) as u64
}
fn level(size: u64) -> Result<u8> {
    for n in 0..=5 {
        if size <= capacity(n) {
            return Ok(n);
        }
    }
    bail!("file exceeds supported logical size")
}
struct Sealer<'a> {
    view: &'a VfsView,
    c: &'a Connection,
    n: &'a Node,
    limit: u64,
    store: StagedContentStore<'a>,
    meter: Option<&'a WorkMeter<'a>>,
    collector: Option<&'a capture::Collector<'a>>,
    metadata: BTreeMap<ObjectRef, FileNode>,
    metadata_order: VecDeque<ObjectRef>,
    zeros: BTreeMap<(u64, bool), ObjectRef>,
}
impl Sealer<'_> {
    fn node(&mut self, r: &ObjectRef) -> Result<FileNode> {
        if let Some(n) = self.metadata.get(r) {
            return Ok(n.clone());
        }
        let bytes = if let Some(collector) = self.collector {
            collector.read_metadata(r)?
        } else {
            if let Some(meter) = self.meter {
                meter.metadata(r.size_bytes)?;
            }
            self.store.read_object(r)?
        };
        let n = cas::decode_file_node(&bytes)?;
        if self.metadata.len() >= 128 {
            if let Some(old) = self.metadata_order.pop_front() {
                self.metadata.remove(&old);
            }
        }
        self.metadata.insert(r.clone(), n.clone());
        self.metadata_order.push_back(r.clone());
        Ok(n)
    }
    fn existing(&mut self, start: u64, length: u64) -> Result<Option<ObjectRef>> {
        let Some(mut object) = self.n.base.clone() else {
            return Ok(None);
        };
        let mut at = 0;
        let mut span = match self.collector {
            Some(collector) => cas::file_info(collector, &object)?.logical_size,
            None => cas::file_info(&Reader(self.view, self.meter), &object)?.logical_size,
        };
        let end = start.checked_add(length).context("seal range overflow")?;
        loop {
            if start == at && length == span {
                return Ok(Some(object));
            }
            if object.kind == ObjectKind::Blob
                || end > at.checked_add(span).context("existing span overflow")?
                || start < at
            {
                return Ok(None);
            }
            let node = self.node(&object)?;
            let child_capacity = if node.level == 0 {
                cas::CHUNK_BYTES as u64
            } else {
                capacity(node.level - 1)
            };
            let index = ((start - at) / child_capacity) as usize;
            if index >= node.children.len() {
                return Ok(None);
            }
            let child_at = at
                .checked_add(
                    (index as u64)
                        .checked_mul(child_capacity)
                        .context("child offset overflow")?,
                )
                .context("child span overflow")?;
            let child_span = node.child_span(index)?;
            if end
                > child_at
                    .checked_add(child_span)
                    .context("child end overflow")?
            {
                return Ok(None);
            }
            object = node.children[index].clone();
            at = child_at;
            span = child_span;
        }
    }
    fn dirty(&self, start: u64, length: u64) -> Result<bool> {
        if length == 0 {
            return Ok(false);
        }
        let last = start
            .checked_add(length - 1)
            .context("dirty range overflow")?
            / BLOCK;
        Ok(self.c.query_row("SELECT EXISTS(SELECT 1 FROM file_blocks WHERE inode=? AND block_index>=? AND block_index<=?)",params![sql_id(self.n.attr.ino),sql_id(start/BLOCK),sql_id(last)],|r|r.get(0))?)
    }
    fn build(&mut self, start: u64, length: u64, file: bool) -> Result<ObjectRef> {
        if let Some(meter) = self.meter {
            meter.check()?;
        }
        let dirty = self.dirty(start, length)?;
        if !dirty
            && start
                .checked_add(length)
                .is_some_and(|end| end <= self.limit)
        {
            if let Some(existing) = self.existing(start, length)? {
                if (file && existing.kind == ObjectKind::File)
                    || (!file && existing.kind == ObjectKind::Blob)
                {
                    return Ok(existing);
                }
            }
        }
        let zero = !dirty && start >= self.limit;
        if zero {
            if let Some(r) = self.zeros.get(&(length, file)) {
                return Ok(r.clone());
            }
        }
        let object = if !file {
            ensure!(
                length <= cas::CHUNK_BYTES as u64,
                "chunk buffer exceeds fixed bound"
            );
            let bytes = if zero {
                vec![0; length as usize]
            } else {
                read_metered(
                    self.view,
                    self.c,
                    self.n,
                    start,
                    length as usize,
                    self.meter,
                )?
            };
            ensure!(
                bytes.len() as u64 == length,
                "seal content range ended early"
            );
            if let Some(meter) = self.meter {
                meter.content(bytes.len() as u64)?;
            }
            match self.collector {
                Some(collector) => collector.put_blob(&bytes)?,
                None => self.store.put_blob(&bytes)?,
            }
        } else {
            let level = level(length)?;
            let span = if level == 0 {
                cas::CHUNK_BYTES as u64
            } else {
                capacity(level - 1)
            };
            let count = (length - 1) / span + 1;
            ensure!(
                count <= cas::FANOUT as u64,
                "seal fanout exceeds fixed bound"
            );
            let mut children = Vec::with_capacity(count as usize);
            for index in 0..count {
                let child_start = start
                    .checked_add(
                        index
                            .checked_mul(span)
                            .context("seal child offset overflow")?,
                    )
                    .context("seal child range overflow")?;
                let child_length = (length - index * span).min(span);
                children.push(self.build(child_start, child_length, level > 0)?);
            }
            let node = FileNode {
                format_version: 1,
                layout: cas::FILE_LAYOUT.into(),
                level,
                logical_size: length,
                children,
            };
            node.validate()?;
            if let Some(meter) = self.meter {
                meter.metadata(canonical_json(&node)?.len() as u64)?;
            }
            match self.collector {
                Some(collector) => collector.put_file_node(&node)?,
                None => self.store.put_file_node(&node)?,
            }
        };
        if zero {
            self.zeros.insert((length, file), object.clone());
        }
        Ok(object)
    }
}
