//! Bounded immutable file contents. An ObjectRef always hashes encoded object
//! bytes; a File reference hashes a descriptor, never the reconstructed file.
//! Callers hold staging/retention across imports and reads. These helpers do
//! not publish roots, stop writers, or assert remote/local durability.

use crate::protocol::{canonical_json, ObjectKind, ObjectRef};
use crate::store::LocalStore;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};

pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;
pub const FANOUT: usize = 256;
pub const MAX_FILE_NODE_BYTES: usize = 64 * 1024;
pub const MAX_RANGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_LOGICAL_BYTES: u64 = i64::MAX as u64;
pub const FILE_LAYOUT: &str = "fixed_4mib_fanout256_v1";
const MAX_LEVEL: u8 = 5;
const MAX_VALIDATION_NODES: usize = 100_000;

/// Level zero holds raw chunks. Higher levels hold File descriptors. All
/// children except the last span their full capacity; a compact final subtree
/// uses its own minimal level rather than a padded single-child branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileNode {
    pub format_version: u32,
    pub layout: String,
    pub level: u8,
    pub logical_size: u64,
    pub children: Vec<ObjectRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileEncoding {
    Blob,
    Chunked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileInfo {
    pub logical_size: u64,
    pub encoding: FileEncoding,
}

/// Small boundary for a later remote resolver. Implementations verify returned
/// bytes and keep an object protected against eviction while it is read. The
/// default methods suit in-memory/test stores; LocalStore overrides raw ranges
/// and copying so even large legacy blobs need no whole-file Vec.
pub trait ContentStore {
    fn put_blob(&self, bytes: &[u8]) -> Result<ObjectRef>;
    fn put_file_node(&self, node: &FileNode) -> Result<ObjectRef>;
    fn read_object(&self, reference: &ObjectRef) -> Result<Vec<u8>>;

    fn read_blob_range(
        &self,
        reference: &ObjectRef,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        validate_blob(reference)?;
        let count = range_length(reference.size_bytes, offset, length)?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let bytes = self.read_object(reference)?;
        reference.verify(&bytes)?;
        let start = usize::try_from(offset)?;
        Ok(bytes[start..start + count].to_vec())
    }

    fn copy_blob_to(&self, reference: &ObjectRef, writer: &mut dyn Write) -> Result<()> {
        validate_blob(reference)?;
        let bytes = self.read_object(reference)?;
        reference.verify(&bytes)?;
        writer.write_all(&bytes)?;
        Ok(())
    }
}

impl ContentStore for LocalStore {
    fn put_blob(&self, _: &[u8]) -> Result<ObjectRef> {
        bail!("file import requires an explicit StagedContentStore")
    }
    fn put_file_node(&self, _: &FileNode) -> Result<ObjectRef> {
        bail!("file import requires an explicit StagedContentStore")
    }
    fn read_object(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        LocalStore::read_object(self, reference)
    }
    fn read_blob_range(
        &self,
        reference: &ObjectRef,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        LocalStore::read_blob_range(self, reference, offset, length)
    }
    fn copy_blob_to(&self, reference: &ObjectRef, writer: &mut dyn Write) -> Result<()> {
        LocalStore::copy_blob_to(self, reference, writer)
    }
}

/// Explicit caller-owned write retention. This wrapper neither starts nor
/// finishes a lease. Every write rechecks live staging under the store/GC lock.
pub struct StagedContentStore<'a> {
    store: &'a LocalStore,
    lease_id: &'a str,
}
impl<'a> StagedContentStore<'a> {
    pub fn new(store: &'a LocalStore, lease_id: &'a str) -> Result<Self> {
        ensure!(
            store.retention()?.staging.contains_key(lease_id),
            "unknown content staging lease"
        );
        Ok(Self { store, lease_id })
    }
    fn receive(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        let reference = ObjectRef::from_bytes(kind, bytes);
        self.store
            .receive_staged(self.lease_id, &reference, bytes)?;
        Ok(reference)
    }
}
impl ContentStore for StagedContentStore<'_> {
    fn put_blob(&self, bytes: &[u8]) -> Result<ObjectRef> {
        self.receive(ObjectKind::Blob, bytes)
    }
    fn put_file_node(&self, node: &FileNode) -> Result<ObjectRef> {
        node.validate()?;
        self.receive(ObjectKind::File, &canonical_json(node)?)
    }
    fn read_object(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.store.read_object(reference)
    }
    fn read_blob_range(
        &self,
        reference: &ObjectRef,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        self.store.read_blob_range(reference, offset, length)
    }
    fn copy_blob_to(&self, reference: &ObjectRef, writer: &mut dyn Write) -> Result<()> {
        self.store.copy_blob_to(reference, writer)
    }
}

fn capacity(level: u8) -> Result<u64> {
    ensure!(
        level <= MAX_LEVEL,
        "file tree level exceeds supported logical size"
    );
    let span = (CHUNK_BYTES as u128)
        .checked_mul(
            (FANOUT as u128)
                .checked_pow(u32::from(level) + 1)
                .context("file tree capacity overflow")?,
        )
        .context("file tree capacity overflow")?;
    Ok(span.min(u128::from(MAX_LOGICAL_BYTES)) as u64)
}

fn minimal_level(size: u64) -> Result<u8> {
    ensure!(
        size > 0 && size <= MAX_LOGICAL_BYTES,
        "invalid chunked file logical size"
    );
    for level in 0..=MAX_LEVEL {
        if size <= capacity(level)? {
            return Ok(level);
        }
    }
    bail!("file exceeds supported logical size")
}

fn validate_blob(reference: &ObjectRef) -> Result<()> {
    reference.validate()?;
    ensure!(
        reference.kind == ObjectKind::Blob,
        "expected raw blob content"
    );
    ensure!(
        reference.size_bytes <= MAX_LOGICAL_BYTES,
        "blob exceeds supported logical file size"
    );
    Ok(())
}

impl FileNode {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1 && self.layout == FILE_LAYOUT,
            "unsupported file layout"
        );
        ensure!(
            self.level == minimal_level(self.logical_size)?,
            "file tree is not minimally shaped"
        );
        ensure!(
            !self.children.is_empty() && self.children.len() <= FANOUT,
            "invalid file child count"
        );
        let child_capacity = if self.level == 0 {
            CHUNK_BYTES as u64
        } else {
            capacity(self.level - 1)?
        };
        let expected_count = (self.logical_size - 1) / child_capacity + 1;
        ensure!(
            self.children.len() as u64 == expected_count,
            "file children do not cover logical span"
        );
        for (index, child) in self.children.iter().enumerate() {
            child.validate()?;
            if self.level == 0 {
                ensure!(
                    child.kind == ObjectKind::Blob && child.size_bytes == self.child_span(index)?,
                    "invalid file chunk kind or length"
                );
            } else {
                ensure!(
                    child.kind == ObjectKind::File
                        && child.size_bytes > 0
                        && child.size_bytes <= MAX_FILE_NODE_BYTES as u64,
                    "invalid file descriptor child"
                );
            }
        }
        ensure!(
            canonical_json(self)?.len() <= MAX_FILE_NODE_BYTES,
            "file descriptor exceeds metadata limit"
        );
        Ok(())
    }

    /// Expected logical length of this position, independent of the encoded
    /// metadata length in its ObjectRef. Parent validity is checked separately.
    pub fn child_span(&self, index: usize) -> Result<u64> {
        ensure!(index < self.children.len(), "file child index out of range");
        let child_capacity = if self.level == 0 {
            CHUNK_BYTES as u64
        } else {
            capacity(self.level - 1)?
        };
        let start = child_capacity
            .checked_mul(index.try_into()?)
            .context("file child offset overflow")?;
        ensure!(start < self.logical_size, "file child exceeds logical span");
        Ok((self.logical_size - start).min(child_capacity))
    }
}

pub fn decode_file_node(bytes: &[u8]) -> Result<FileNode> {
    ensure!(
        bytes.len() <= MAX_FILE_NODE_BYTES,
        "file descriptor exceeds metadata limit"
    );
    let node: FileNode = serde_json::from_slice(bytes)?;
    node.validate()?;
    ensure!(
        canonical_json(&node)? == bytes,
        "file descriptor must use canonical compact JSON"
    );
    Ok(node)
}

pub fn file_node_references(bytes: &[u8]) -> Result<Vec<ObjectRef>> {
    Ok(decode_file_node(bytes)?.children)
}

/// Closure publication must check this for EVERY parent-child descriptor edge.
/// Validating either standalone object cannot establish the child's span.
pub fn validate_file_child(parent: &FileNode, index: usize, child: &FileNode) -> Result<()> {
    parent.validate()?;
    child.validate()?;
    ensure!(
        parent.level > 0,
        "raw chunks have no file descriptor children"
    );
    let span = parent.child_span(index)?;
    ensure!(
        child.logical_size == span && child.level == minimal_level(span)?,
        "file child logical span or level differs from parent"
    );
    Ok(())
}

fn read_node(store: &(impl ContentStore + ?Sized), reference: &ObjectRef) -> Result<FileNode> {
    reference.validate()?;
    ensure!(
        reference.kind == ObjectKind::File
            && reference.size_bytes > 0
            && reference.size_bytes <= MAX_FILE_NODE_BYTES as u64,
        "invalid file descriptor reference"
    );
    let bytes = store.read_object(reference)?;
    reference.verify(&bytes)?;
    decode_file_node(&bytes)
}

/// Reads only root metadata. This is not proof of a complete retained closure
/// or of child availability; validate_file_tree and store publication do those
/// structural/retention checks respectively.
pub fn file_info(store: &(impl ContentStore + ?Sized), content: &ObjectRef) -> Result<FileInfo> {
    if content.kind == ObjectKind::Blob {
        validate_blob(content)?;
        Ok(FileInfo {
            logical_size: content.size_bytes,
            encoding: FileEncoding::Blob,
        })
    } else {
        let node = read_node(store, content)?;
        Ok(FileInfo {
            logical_size: node.logical_size,
            encoding: FileEncoding::Chunked,
        })
    }
}

#[derive(Clone)]
struct Part {
    reference: ObjectRef,
    size: u64,
}
struct Builder<'a, S: ContentStore + ?Sized> {
    store: &'a S,
    frontier: Vec<Vec<Part>>,
    total: u64,
}
impl<'a, S: ContentStore + ?Sized> Builder<'a, S> {
    fn push(&mut self, level: usize, part: Part) -> Result<()> {
        ensure!(
            level <= usize::from(MAX_LEVEL) + 1,
            "file tree frontier exceeds supported size"
        );
        while self.frontier.len() <= level {
            self.frontier.push(Vec::new());
        }
        self.frontier[level].push(part);
        if self.frontier[level].len() == FANOUT {
            let parts = std::mem::take(&mut self.frontier[level]);
            let parent = self.node(level as u8, parts)?;
            self.push(level + 1, parent)?;
        }
        Ok(())
    }
    fn node(&self, level: u8, parts: Vec<Part>) -> Result<Part> {
        let size = parts.iter().try_fold(0_u64, |sum, part| {
            sum.checked_add(part.size)
                .context("file logical size overflow")
        })?;
        let node = FileNode {
            format_version: 1,
            layout: FILE_LAYOUT.into(),
            level,
            logical_size: size,
            children: parts.into_iter().map(|part| part.reference).collect(),
        };
        node.validate()?;
        let reference = self.store.put_file_node(&node)?;
        ensure!(
            reference.kind == ObjectKind::File,
            "store returned another descriptor kind"
        );
        reference.verify(&canonical_json(&node)?)?;
        Ok(Part { reference, size })
    }
    fn chunk(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= CHUNK_BYTES,
            "invalid imported chunk length"
        );
        self.total = self
            .total
            .checked_add(bytes.len() as u64)
            .context("file logical size overflow")?;
        ensure!(
            self.total <= MAX_LOGICAL_BYTES,
            "file exceeds supported logical size"
        );
        let reference = self.store.put_blob(bytes)?;
        validate_blob(&reference)?;
        reference.verify(bytes)?;
        self.push(
            0,
            Part {
                reference,
                size: bytes.len() as u64,
            },
        )
    }
    fn finish(mut self) -> Result<ObjectRef> {
        let mut level = 0;
        while level < self.frontier.len() {
            let parts = std::mem::take(&mut self.frontier[level]);
            if parts.is_empty() {
                level += 1;
                continue;
            }
            // A raw chunk still needs a leaf descriptor inside a chunked file.
            let part = if level > 0 && parts.len() == 1 {
                parts.into_iter().next().unwrap()
            } else {
                self.node(level as u8, parts)?
            };
            if self.frontier.iter().skip(level + 1).all(Vec::is_empty) {
                ensure!(
                    part.size == self.total,
                    "file frontier lost logical content"
                );
                return Ok(part.reference);
            }
            self.push(level + 1, part)?;
            level += 1;
        }
        bail!("empty chunked file frontier")
    }
}

fn fill(reader: &mut (impl Read + ?Sized), buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(filled)
}

/// Stream a quiesced file with one 4 MiB data buffer and a bounded tree frontier.
/// Up to 4 MiB is stored as a legacy Blob. Failed imports leave only uncommitted
/// staged objects; the caller owns abandonment and durable root publication.
pub fn import_file(
    store: &(impl ContentStore + ?Sized),
    reader: &mut (impl Read + ?Sized),
) -> Result<ObjectRef> {
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    let filled = fill(reader, &mut buffer)?;
    let mut extra = [0_u8; 1];
    if filled < CHUNK_BYTES || fill(reader, &mut extra)? == 0 {
        let reference = store.put_blob(&buffer[..filled])?;
        validate_blob(&reference)?;
        reference.verify(&buffer[..filled])?;
        return Ok(reference);
    }
    let mut builder = Builder {
        store,
        frontier: Vec::new(),
        total: 0,
    };
    builder.chunk(&buffer)?;
    buffer[0] = extra[0];
    let mut carried = 1;
    loop {
        let filled = carried + fill(reader, &mut buffer[carried..])?;
        if filled == 0 {
            break;
        }
        builder.chunk(&buffer[..filled])?;
        if filled < CHUNK_BYTES {
            break;
        }
        carried = 0;
    }
    builder.finish()
}

fn range_length(size: u64, offset: u64, length: usize) -> Result<usize> {
    ensure!(
        length <= MAX_RANGE_BYTES,
        "file read exceeds bounded range limit"
    );
    let end = offset
        .checked_add(length.try_into()?)
        .context("file read range overflow")?;
    if offset >= size {
        return Ok(0);
    }
    Ok(usize::try_from(end.min(size) - offset)?)
}

fn read_node_range(
    store: &(impl ContentStore + ?Sized),
    node: &FileNode,
    offset: u64,
    count: usize,
    output: &mut Vec<u8>,
) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    let child_capacity = if node.level == 0 {
        CHUNK_BYTES as u64
    } else {
        capacity(node.level - 1)?
    };
    let end = offset
        .checked_add(count.try_into()?)
        .context("file read range overflow")?;
    let first = usize::try_from(offset / child_capacity)?;
    let last = usize::try_from((end - 1) / child_capacity)?;
    for index in first..=last {
        let start = child_capacity
            .checked_mul(index.try_into()?)
            .context("file child offset overflow")?;
        let local_start = offset.saturating_sub(start);
        let local_end = (end - start).min(node.child_span(index)?);
        let length = usize::try_from(local_end - local_start)?;
        let reference = &node.children[index];
        if node.level == 0 {
            let bytes = store.read_blob_range(reference, local_start, length)?;
            ensure!(
                bytes.len() == length,
                "store returned a short verified file range"
            );
            output.extend_from_slice(&bytes);
        } else {
            let child = read_node(store, reference)?;
            validate_file_child(node, index, &child)?;
            read_node_range(store, &child, local_start, length, output)?;
        }
    }
    Ok(())
}

pub fn read_file_range(
    store: &(impl ContentStore + ?Sized),
    content: &ObjectRef,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    if content.kind == ObjectKind::Blob {
        validate_blob(content)?;
        let count = range_length(content.size_bytes, offset, length)?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let bytes = store.read_blob_range(content, offset, count)?;
        ensure!(
            bytes.len() == count,
            "store returned a short verified blob range"
        );
        return Ok(bytes);
    }
    let node = read_node(store, content)?;
    let count = range_length(node.logical_size, offset, length)?;
    let mut output = Vec::with_capacity(count);
    read_node_range(store, &node, offset, count, &mut output)?;
    ensure!(
        output.len() == count,
        "file tree returned an incomplete range"
    );
    Ok(output)
}

/// Validate all descriptor edges, memoizing repeated immutable subtrees. This
/// checks shape/spans, not chunk availability or durable retention. Full store
/// closure publication still verifies every referenced chunk before ack.
pub fn validate_file_tree(
    store: &(impl ContentStore + ?Sized),
    content: &ObjectRef,
) -> Result<FileInfo> {
    if content.kind == ObjectKind::Blob {
        return file_info(store, content);
    }
    let root = read_node(store, content)?;
    let info = FileInfo {
        logical_size: root.logical_size,
        encoding: FileEncoding::Chunked,
    };
    let mut pending = vec![(content.clone(), root.level, root.logical_size)];
    let mut root = Some(root);
    let mut seen = BTreeMap::new();
    while let Some((reference, level, size)) = pending.pop() {
        let shape = (reference.clone(), level, size);
        if let Some(previous) = seen.get(&reference.digest) {
            ensure!(
                *previous == shape,
                "repeated file descriptor has incompatible expected shape"
            );
            continue;
        }
        seen.insert(reference.digest.clone(), shape);
        ensure!(
            seen.len() <= MAX_VALIDATION_NODES,
            "file descriptor graph exceeds validation limit"
        );
        let node = if let Some(root) = root.take() {
            root
        } else {
            read_node(store, &reference)?
        };
        ensure!(
            node.level == level && node.logical_size == size,
            "file child logical span or level differs from parent"
        );
        if node.level > 0 {
            for (index, reference) in node.children.iter().enumerate() {
                let span = node.child_span(index)?;
                pending.push((reference.clone(), minimal_level(span)?, span));
            }
        }
    }
    Ok(info)
}

fn export_node(
    store: &(impl ContentStore + ?Sized),
    node: &FileNode,
    writer: &mut dyn Write,
) -> Result<()> {
    for (index, reference) in node.children.iter().enumerate() {
        if node.level == 0 {
            store.copy_blob_to(reference, writer)?;
        } else {
            let child = read_node(store, reference)?;
            validate_file_child(node, index, &child)?;
            export_node(store, &child, writer)?;
        }
    }
    Ok(())
}

/// Stream into a caller-owned destination. Errors can leave partial private
/// output; the caller must remove it and must not publish/sync a success record.
pub fn export_file(
    store: &(impl ContentStore + ?Sized),
    content: &ObjectRef,
    writer: &mut dyn Write,
) -> Result<FileInfo> {
    if content.kind == ObjectKind::Blob {
        let info = file_info(store, content)?;
        store.copy_blob_to(content, writer)?;
        return Ok(info);
    }
    let node = read_node(store, content)?;
    export_node(store, &node, writer)?;
    Ok(FileInfo {
        logical_size: node.logical_size,
        encoding: FileEncoding::Chunked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct DescriptorStore {
        objects: RefCell<BTreeMap<String, Vec<u8>>>,
        reads: Cell<usize>,
    }
    impl ContentStore for DescriptorStore {
        fn put_blob(&self, _: &[u8]) -> Result<ObjectRef> {
            bail!("synthetic frontier has no chunk bytes")
        }
        fn put_file_node(&self, node: &FileNode) -> Result<ObjectRef> {
            let bytes = canonical_json(node)?;
            let reference = ObjectRef::from_bytes(ObjectKind::File, &bytes);
            self.objects
                .borrow_mut()
                .insert(reference.digest.clone(), bytes);
            Ok(reference)
        }
        fn read_object(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
            self.reads.set(self.reads.get() + 1);
            self.objects
                .borrow()
                .get(&reference.digest)
                .cloned()
                .context("descriptor missing")
        }
    }

    #[test]
    fn frontier_carries_and_compacts_right_edges_without_padding_or_file_buffers() {
        for chunks in [2_u64, 255, 256, 257, 65_535, 65_536, 65_537] {
            let store = DescriptorStore::default();
            let mut builder = Builder {
                store: &store,
                frontier: Vec::new(),
                total: 0,
            };
            for index in 0..chunks {
                let size = if index + 1 == chunks {
                    7
                } else {
                    CHUNK_BYTES as u64
                };
                let reference = ObjectRef {
                    kind: ObjectKind::Blob,
                    algorithm: crate::protocol::HashAlgorithm::Sha256,
                    digest: "a".repeat(64),
                    size_bytes: size,
                };
                builder.total += size;
                builder.push(0, Part { reference, size }).unwrap();
                assert!(
                    builder.frontier.iter().map(Vec::len).sum::<usize>()
                        <= FANOUT * (usize::from(MAX_LEVEL) + 1)
                );
            }
            let size = builder.total;
            let root = builder.finish().unwrap();
            let node = read_node(&store, &root).unwrap();
            assert_eq!(node.level, minimal_level(size).unwrap());
            assert_eq!(node.logical_size, size);
            store.reads.set(0);
            validate_file_tree(&store, &root).unwrap();
            assert_eq!(
                store.reads.get(),
                store.objects.borrow().len(),
                "shared descriptors are read once during validation"
            );
            assert!(store
                .objects
                .borrow()
                .values()
                .all(|bytes| bytes.len() <= MAX_FILE_NODE_BYTES));
        }
    }
}
