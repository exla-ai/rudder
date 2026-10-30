//! Metered immutable CAS reads and fallible streaming writes to the pinned Git
//! object database. Called only by a background bridge/helper, never App render.

use anyhow::{Context, Result, bail, ensure};
use gix::objs::Write as _;
use jj_lib::backend::{FileId, TreeValue};
use jj_lib::git_backend::GitBackend;
use jj_lib::store::Store;
use rudder_engine::content::{self, ContentStore, FileNode};
use rudder_engine::directory::DirectoryStore;
use rudder_engine::protocol::{ObjectKind, ObjectRef};
use rudder_engine::store::LocalStore;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionBudget {
    pub max_entries: u64,
    pub max_metadata_bytes: u64,
    pub max_content_bytes: u64,
    /// Pinned jj's conflict and reverse-reader APIs can buffer entire blobs.
    pub max_jj_buffer_bytes: u64,
    pub max_depth: usize,
    pub max_elapsed_ms: u64,
}
impl Default for ProjectionBudget {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_metadata_bytes: 128 * 1024 * 1024,
            max_content_bytes: 256 * 1024 * 1024,
            max_jj_buffer_bytes: 32 * 1024 * 1024,
            max_depth: 256,
            max_elapsed_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionWork {
    pub source_entries_visited: u64,
    pub jj_entries_visited: u64,
    pub cas_metadata_reads: u64,
    pub cas_metadata_bytes: u64,
    pub plan_metadata_bytes_admitted: u64,
    pub cas_content_bytes: u64,
    pub jj_buffer_bytes_admitted: u64,
    /// Logical bytes submitted to Git; deduplication can avoid physical writes.
    pub git_blob_bytes_submitted: u64,
    pub reused_leaf_values: u64,
}
struct Meter {
    budget: ProjectionBudget,
    started: Instant,
    work: ProjectionWork,
}
#[derive(Clone)]
pub(crate) struct MeteredSource {
    pub store: LocalStore,
    meter: Arc<Mutex<Meter>>,
}
impl MeteredSource {
    pub fn new(store: LocalStore, budget: ProjectionBudget) -> Result<Self> {
        ensure!(
            budget.max_elapsed_ms > 0 && budget.max_depth <= 256,
            "invalid projection budget"
        );
        Ok(Self {
            store,
            meter: Arc::new(Mutex::new(Meter {
                budget,
                started: Instant::now(),
                work: ProjectionWork::default(),
            })),
        })
    }
    fn update(&self, change: impl FnOnce(&mut Meter) -> Result<()>) -> Result<()> {
        let mut meter = self
            .meter
            .lock()
            .map_err(|_| anyhow::anyhow!("projection meter poisoned"))?;
        ensure!(
            meter.started.elapsed().as_millis() < u128::from(meter.budget.max_elapsed_ms),
            "projection cooperative deadline exceeded"
        );
        change(&mut meter)
    }
    pub fn work(&self) -> ProjectionWork {
        self.meter.lock().unwrap().work.clone()
    }
    pub fn depth(&self, depth: usize) -> Result<()> {
        self.update(|m| {
            ensure!(
                depth <= m.budget.max_depth,
                "projection depth budget exceeded"
            );
            Ok(())
        })
    }
    pub fn entry(&self, source: bool, bytes: usize) -> Result<()> {
        self.update(|m| {
            if source {
                m.work.source_entries_visited += 1;
            } else {
                m.work.jj_entries_visited += 1;
            }
            ensure!(
                m.work.source_entries_visited + m.work.jj_entries_visited <= m.budget.max_entries,
                "projection entry budget exceeded"
            );
            m.work.plan_metadata_bytes_admitted = m
                .work
                .plan_metadata_bytes_admitted
                .checked_add(bytes as u64)
                .context("plan count overflow")?;
            ensure!(
                m.work.plan_metadata_bytes_admitted <= m.budget.max_metadata_bytes,
                "projection plan exceeds metadata budget"
            );
            Ok(())
        })
    }
    pub fn metadata(&self, bytes: u64) -> Result<()> {
        self.update(|m| {
            m.work.cas_metadata_reads += 1;
            m.work.cas_metadata_bytes = m
                .work
                .cas_metadata_bytes
                .checked_add(bytes)
                .context("metadata count overflow")?;
            ensure!(
                m.work.cas_metadata_bytes <= m.budget.max_metadata_bytes,
                "projection metadata budget exceeded"
            );
            Ok(())
        })
    }
    pub fn content(&self, bytes: u64) -> Result<()> {
        self.update(|m| {
            m.work.cas_content_bytes = m
                .work
                .cas_content_bytes
                .checked_add(bytes)
                .context("content count overflow")?;
            ensure!(
                m.work.cas_content_bytes <= m.budget.max_content_bytes,
                "projection content budget exceeded"
            );
            Ok(())
        })
    }
    pub fn admit_logical_bytes(&self, size: u64) -> Result<()> {
        self.update(|m| {
            ensure!(
                size <= m
                    .budget
                    .max_content_bytes
                    .saturating_sub(m.work.cas_content_bytes),
                "file exceeds remaining projection content budget"
            );
            Ok(())
        })
    }
    pub fn admit_jj_buffer(&self, size: u64, copies: u64) -> Result<()> {
        self.update(|m| {
            ensure!(
                size <= m.budget.max_jj_buffer_bytes,
                "jj blob exceeds bounded materialization profile"
            );
            m.work.jj_buffer_bytes_admitted = m
                .work
                .jj_buffer_bytes_admitted
                .checked_add(size.checked_mul(copies).context("jj admission overflow")?)
                .context("jj admission overflow")?;
            ensure!(
                m.work.jj_buffer_bytes_admitted <= m.budget.max_content_bytes,
                "jj materialization admission budget exceeded"
            );
            Ok(())
        })
    }
    pub fn git_written(&self, size: u64) -> Result<()> {
        self.update(|m| {
            m.work.git_blob_bytes_submitted = m
                .work
                .git_blob_bytes_submitted
                .checked_add(size)
                .context("Git byte count overflow")?;
            Ok(())
        })
    }
    pub fn reused(&self) {
        self.meter.lock().unwrap().work.reused_leaf_values += 1;
    }
    pub fn read_metadata(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.metadata(reference.size_bytes)?;
        self.store.read_object(reference)
    }
    pub fn read_all_file(&self, reference: &ObjectRef, logical_size: u64) -> Result<Vec<u8>> {
        self.admit_logical_bytes(logical_size)?;
        self.admit_jj_buffer(logical_size, 1)?;
        let mut reader = self.file_reader(reference, logical_size)?;
        let mut bytes = Vec::with_capacity(usize::try_from(logical_size)?);
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    pub fn file_reader(
        &self,
        reference: &ObjectRef,
        logical_size: u64,
    ) -> Result<ExactReader<Box<dyn Read + Send>>> {
        let info = content::file_info(self, reference)?;
        ensure!(
            info.logical_size == logical_size,
            "source entry logical size differs from verified file descriptor"
        );
        self.admit_logical_bytes(logical_size)?;
        if logical_size == 0 && reference.kind == ObjectKind::Blob {
            self.store.read_blob_range(reference, 0, 0)?;
        }
        let reader: Box<dyn Read + Send> =
            if reference.kind == ObjectKind::Blob && logical_size > 4 * 1024 * 1024 {
                let (tx, rx) = mpsc::sync_channel(2);
                let source = self.clone();
                let object = reference.clone();
                std::thread::spawn(move || {
                    let mut writer = ChannelWriter { tx: tx.clone() };
                    let result = ContentStore::copy_blob_to(&source, &object, &mut writer)
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(StreamPart::End(result));
                });
                Box::new(ChannelReader {
                    rx,
                    current: io::Cursor::new(Vec::new()),
                    ended: false,
                })
            } else {
                Box::new(RangeReader {
                    source: self.clone(),
                    object: reference.clone(),
                    logical_size,
                    offset: 0,
                    current: io::Cursor::new(Vec::new()),
                })
            };
        Ok(ExactReader {
            inner: reader,
            remaining: logical_size,
            ended: false,
        })
    }
}
impl DirectoryStore for MeteredSource {
    fn read(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.read_metadata(reference)
    }
    fn put(&self, _: ObjectKind, _: &[u8]) -> Result<ObjectRef> {
        bail!("source projection reader cannot write CAS metadata")
    }
}
impl ContentStore for MeteredSource {
    fn put_blob(&self, _: &[u8]) -> Result<ObjectRef> {
        bail!("source projection reader cannot write CAS blobs")
    }
    fn put_file_node(&self, _: &FileNode) -> Result<ObjectRef> {
        bail!("source projection reader cannot write CAS files")
    }
    fn read_object(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        if reference.kind == ObjectKind::Blob {
            self.content(reference.size_bytes)?;
            self.store.read_object(reference)
        } else {
            self.read_metadata(reference)
        }
    }
    fn read_blob_range(
        &self,
        reference: &ObjectRef,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        self.content(reference.size_bytes)?;
        self.store.read_blob_range(reference, offset, length)
    }
    fn copy_blob_to(&self, reference: &ObjectRef, writer: &mut dyn Write) -> Result<()> {
        self.content(reference.size_bytes)?;
        self.store.copy_blob_to(reference, writer)
    }
}

struct RangeReader {
    source: MeteredSource,
    object: ObjectRef,
    logical_size: u64,
    offset: u64,
    current: io::Cursor<Vec<u8>>,
}
impl Read for RangeReader {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if target.is_empty() {
            return Ok(0);
        }
        if self.current.position() as usize == self.current.get_ref().len() {
            if self.offset == self.logical_size {
                return Ok(0);
            }
            let bytes = content::read_file_range(
                &self.source,
                &self.object,
                self.offset,
                (self.logical_size - self.offset).min(4 * 1024 * 1024) as usize,
            )
            .map_err(io_error)?;
            if bytes.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "verified CAS returned a short file",
                ));
            }
            self.offset += bytes.len() as u64;
            self.current = io::Cursor::new(bytes);
        }
        self.current.read(target)
    }
}
pub(crate) struct ExactReader<R> {
    inner: R,
    remaining: u64,
    ended: bool,
}
impl<R: Read> Read for ExactReader<R> {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if target.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            if self.ended {
                return Ok(0);
            }
            let mut extra = [0];
            if self.inner.read(&mut extra)? != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "source exceeded declared file length",
                ));
            }
            self.ended = true;
            return Ok(0);
        }
        let length = usize::try_from(self.remaining.min(target.len() as u64)).unwrap();
        let count = self.inner.read(&mut target[..length])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source shorter than declared file length",
            ));
        }
        self.remaining -= count as u64;
        Ok(count)
    }
}
enum StreamPart {
    Data(Vec<u8>),
    End(std::result::Result<(), String>),
}
struct ChannelWriter {
    tx: mpsc::SyncSender<StreamPart>,
}
impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for chunk in bytes.chunks(64 * 1024) {
            self.tx
                .send(StreamPart::Data(chunk.to_vec()))
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "projection reader stopped")
                })?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct ChannelReader {
    rx: mpsc::Receiver<StreamPart>,
    current: io::Cursor<Vec<u8>>,
    ended: bool,
}
impl Read for ChannelReader {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if target.is_empty() {
            return Ok(0);
        }
        loop {
            let count = self.current.read(target)?;
            if count > 0 || self.ended {
                return Ok(count);
            }
            match self.rx.recv().map_err(|_| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "CAS exporter disappeared")
            })? {
                StreamPart::Data(bytes) => self.current = io::Cursor::new(bytes),
                StreamPart::End(Ok(())) => self.ended = true,
                StreamPart::End(Err(error)) => return Err(io::Error::other(error)),
            }
        }
    }
}

pub(crate) fn git_backend(store: &Store) -> Result<&GitBackend> {
    store
        .backend_impl::<GitBackend>()
        .context("source projection requires the pinned Git backend")
}
pub(crate) fn write_file(
    source: &MeteredSource,
    store: &Store,
    object: &ObjectRef,
    logical_size: u64,
) -> Result<FileId> {
    let mut reader = source.file_reader(object, logical_size)?;
    let repo = git_backend(store)?.git_repo();
    let id = repo
        .objects
        .write_stream(gix::objs::Kind::Blob, logical_size, &mut reader)
        .map_err(|e| anyhow::anyhow!("Git blob stream failed: {e}"))?;
    ensure!(
        reader.remaining == 0 && reader.ended,
        "Git writer did not consume the complete verified source"
    );
    source.git_written(logical_size)?;
    Ok(FileId::new(id.as_bytes().to_vec()))
}
pub(crate) fn read_git_blob(
    source: &MeteredSource,
    store: &Store,
    id: &[u8],
    copies: u64,
) -> Result<Vec<u8>> {
    let repo = git_backend(store)?.git_repo();
    ensure!(id.len() == 20, "unsupported Git blob identity length");
    let oid = gix::ObjectId::try_from(id)?;
    let header = repo.find_header(oid)?;
    ensure!(header.kind() == gix::objs::Kind::Blob, "expected Git blob");
    source.admit_jj_buffer(header.size(), copies)?;
    Ok(repo.find_object(oid)?.try_into_blob()?.take_data())
}
pub(crate) fn reject_submodule(value: &jj_lib::merge::MergedTreeValue) -> Result<()> {
    ensure!(
        !value
            .iter()
            .flatten()
            .any(|v| matches!(v, TreeValue::GitSubmodule(_))),
        "Git submodules are unsupported by this projection profile"
    );
    Ok(())
}
fn io_error(error: anyhow::Error) -> io::Error {
    io::Error::other(format!("{error:#}"))
}
