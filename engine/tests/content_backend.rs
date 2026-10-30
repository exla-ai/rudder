use anyhow::{bail, Result};
use rudder_engine::content::*;
use rudder_engine::protocol::{canonical_json, ObjectKind, ObjectRef};
use rudder_engine::store::LocalStore;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn node(level: u8, size: u64, children: Vec<ObjectRef>) -> FileNode {
    FileNode {
        format_version: 1,
        layout: FILE_LAYOUT.into(),
        level,
        logical_size: size,
        children,
    }
}
fn put_node(store: &LocalStore, value: &FileNode) -> ObjectRef {
    store.put_metadata(ObjectKind::File, value).unwrap()
}
fn import(store: &LocalStore, reader: &mut impl Read) -> ObjectRef {
    let stage = store.begin_staging(Duration::from_secs(3600)).unwrap();
    let staged = StagedContentStore::new(store, &stage.id).unwrap();
    let root = import_file(&staged, reader).unwrap();
    store
        .publish(&stage.id, &format!("file:{}", root.digest), &root)
        .unwrap();
    root
}

#[test]
fn small_and_boundary_imports_preserve_legacy_blobs_and_exact_lengths() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    for length in [0, 1, CHUNK_BYTES - 1, CHUNK_BYTES, CHUNK_BYTES + 1] {
        let root = import(&store, &mut io::repeat(0x5a).take(length as u64));
        let info = file_info(&store, &root).unwrap();
        assert_eq!(info.logical_size, length as u64);
        assert_eq!(
            root.kind,
            if length <= CHUNK_BYTES {
                ObjectKind::Blob
            } else {
                ObjectKind::File
            }
        );
        assert_eq!(
            info.encoding,
            if length <= CHUNK_BYTES {
                FileEncoding::Blob
            } else {
                FileEncoding::Chunked
            }
        );
        assert_eq!(
            read_file_range(&store, &root, length.saturating_sub(3) as u64, 8).unwrap(),
            vec![0x5a; length.min(3)]
        );
    }
    assert!(
        import_file(&store, &mut Cursor::new(b"unretained")).is_err(),
        "a bare LocalStore cannot silently import without staging"
    );
    assert!(store.retention().unwrap().staging.is_empty());
}

#[test]
fn canonical_nodes_reject_forged_lengths_kinds_levels_and_encoding() {
    let full = ObjectRef::from_bytes(ObjectKind::Blob, &vec![0x61; CHUNK_BYTES]);
    let tail = ObjectRef::from_bytes(ObjectKind::Blob, b"tail");
    let valid = node(0, CHUNK_BYTES as u64 + 4, vec![full.clone(), tail.clone()]);
    let encoded = canonical_json(&valid).unwrap();
    assert_eq!(decode_file_node(&encoded).unwrap(), valid);
    assert_eq!(file_node_references(&encoded).unwrap(), valid.children);
    let mut cases = vec![
        node(0, 0, vec![]),
        node(0, 1, vec![]),
        node(0, CHUNK_BYTES as u64 + 4, vec![tail.clone(), full.clone()]),
        node(0, CHUNK_BYTES as u64 + 5, valid.children.clone()),
        node(1, CHUNK_BYTES as u64 + 4, valid.children.clone()),
        node(255, 1, vec![full.clone()]),
        node(0, MAX_LOGICAL_BYTES + 1, vec![full.clone()]),
        node(0, 1, vec![ObjectRef::from_bytes(ObjectKind::File, b"x")]),
        node(0, 1, vec![full; FANOUT + 1]),
    ];
    let mut bad_layout = valid.clone();
    bad_layout.layout = "future_unqualified".into();
    cases.push(bad_layout);
    let mut bad_version = valid.clone();
    bad_version.format_version = 2;
    cases.push(bad_version);
    for invalid in cases {
        assert!(
            invalid.validate().is_err(),
            "accepted invalid node {invalid:?}"
        );
    }
    let mut spaced = vec![b' '];
    spaced.extend_from_slice(&encoded);
    assert!(decode_file_node(&spaced).is_err());
    assert!(decode_file_node(&vec![b' '; MAX_FILE_NODE_BYTES + 1]).is_err());
    assert!(decode_file_node(br#"{"format_version":1,"format_version":1,"layout":"fixed_4mib_fanout256_v1","level":0,"logical_size":1,"children":[]}"#).is_err());
    let mut unknown: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    unknown["hidden_reference"] = serde_json::json!(tail);
    assert!(decode_file_node(&canonical_json(&unknown).unwrap()).is_err());
}

#[test]
fn capacities_near_signed_offsets_are_checked_without_integer_overflow() {
    let placeholder = ObjectRef::from_bytes(ObjectKind::File, b"{}");
    let largest = node(
        5,
        MAX_LOGICAL_BYTES,
        vec![placeholder.clone(), placeholder.clone()],
    );
    largest.validate().unwrap();
    assert_eq!(largest.child_span(0).unwrap(), 1_u64 << 62);
    assert_eq!(largest.child_span(1).unwrap(), (1_u64 << 62) - 1);
    assert!(largest.child_span(2).is_err());
    let first = node(4, 1_u64 << 62, vec![placeholder.clone(); FANOUT]);
    let last = node(4, (1_u64 << 62) - 1, vec![placeholder.clone(); FANOUT]);
    validate_file_child(&largest, 0, &first).unwrap();
    validate_file_child(&largest, 1, &last).unwrap();
    assert!(validate_file_child(&largest, 1, &first).is_err());
    assert!(node(6, MAX_LOGICAL_BYTES, vec![placeholder])
        .validate()
        .is_err());
}

fn branched_fixture(store: &LocalStore, last_length: usize) -> (FileNode, ObjectRef, ObjectRef) {
    let chunk = store.put_blob(&vec![0x61; CHUNK_BYTES]).unwrap();
    let full = node(0, CHUNK_BYTES as u64 * FANOUT as u64, vec![chunk; FANOUT]);
    let first = put_node(store, &full);
    let tail = store.put_blob(&vec![0x62; last_length]).unwrap();
    let last = put_node(store, &node(0, last_length as u64, vec![tail]));
    let parent = node(
        1,
        CHUNK_BYTES as u64 * FANOUT as u64 + 13,
        vec![first, last.clone()],
    );
    let root = put_node(store, &parent);
    (parent, root, last)
}

#[test]
fn cross_descriptor_validation_precedes_retention_and_random_range_success() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let stage = store.begin_staging(Duration::from_secs(3600)).unwrap();
    let (_, valid, _) = branched_fixture(&store, 13);
    let boundary = CHUNK_BYTES as u64 * FANOUT as u64;
    validate_file_tree(&store, &valid).unwrap();
    assert_eq!(
        read_file_range(&store, &valid, boundary - 8, 21).unwrap(),
        [vec![0x61; 8], vec![0x62; 13]].concat()
    );
    let (parent, invalid, last) = branched_fixture(&store, 12);
    assert!(validate_file_child(
        &parent,
        1,
        &decode_file_node(&store.read_object(&last).unwrap()).unwrap()
    )
    .is_err());
    assert!(validate_file_tree(&store, &invalid).is_err());
    assert!(read_file_range(&store, &invalid, boundary, 13).is_err());
    assert!(store.publish(&stage.id, "forged-span", &invalid).is_err());
    assert!(!store.retention().unwrap().pins.contains_key("forged-span"));
    store.publish(&stage.id, "valid", &valid).unwrap();
    assert!(read_file_range(&store, &valid, u64::MAX, 1).is_err());
    assert!(read_file_range(&store, &valid, 0, MAX_RANGE_BYTES + 1).is_err());
    assert!(read_file_range(&store, &valid, boundary + 100, 7)
        .unwrap()
        .is_empty());
}

#[test]
fn repeated_chunks_share_storage_but_preserve_logical_bytes_and_bounded_reads() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let size = 2 * CHUNK_BYTES;
    let root = import(&store, &mut io::repeat(0x71).take(size as u64));
    let manifest = decode_file_node(&store.read_object(&root).unwrap()).unwrap();
    assert_eq!(manifest.children.len(), 2);
    assert_eq!(manifest.children[0], manifest.children[1]);
    let closure = store.closure(&root).unwrap();
    assert_eq!(closure.len(), 2, "one descriptor and one unique raw chunk");
    assert_eq!(
        closure
            .iter()
            .map(|reference| reference.size_bytes)
            .sum::<u64>(),
        CHUNK_BYTES as u64 + root.size_bytes
    );
    assert_eq!(file_info(&store, &root).unwrap().logical_size, size as u64);
    let metered = Metered::new(&store);
    assert_eq!(
        read_file_range(&metered, &root, CHUNK_BYTES as u64 - 16, 32).unwrap(),
        vec![0x71; 32]
    );
    assert_eq!(metered.metadata_reads.get(), 1);
    assert_eq!(metered.blob_ranges.get(), 2);
    assert!(metered.maximum_blob.get() <= CHUNK_BYTES as u64);
}

#[test]
fn corrupt_missing_chunks_and_missing_descriptors_fail_without_false_publication() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let root = import(&store, &mut io::repeat(0x7a).take(CHUNK_BYTES as u64 + 17));
    let mut manifest = decode_file_node(&store.read_object(&root).unwrap()).unwrap();
    let first = manifest.children.remove(0);
    let path = store.object_path(&first).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(&path, vec![0x7b; CHUNK_BYTES]).unwrap();
    assert!(
        read_file_range(&store, &root, 0, 1).is_err(),
        "never expose an unchecked byte from a corrupt chunk"
    );
    fs::remove_file(path).unwrap();
    assert!(read_file_range(&store, &root, 0, 1).is_err());
    // A request for an unrelated verified tail does not hydrate the missing chunk.
    assert_eq!(
        read_file_range(&store, &root, CHUNK_BYTES as u64, 17).unwrap(),
        vec![0x7a; 17]
    );
    let (_, branch, missing) = branched_fixture(&store, 13);
    fs::remove_file(store.object_path(&missing).unwrap()).unwrap();
    assert!(validate_file_tree(&store, &branch).is_err());
    assert!(read_file_range(&store, &branch, CHUNK_BYTES as u64 * FANOUT as u64, 1).is_err());
}

#[test]
fn import_handles_short_reads_and_interruption_and_fails_closed_on_io_error() {
    struct Short {
        inner: Cursor<Vec<u8>>,
        interrupted: bool,
    }
    impl Read for Short {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = output.len().min(7);
            self.inner.read(&mut output[..count])
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let bytes = b"short and interrupted reads are normal".to_vec();
    let root = import(
        &store,
        &mut Short {
            inner: Cursor::new(bytes.clone()),
            interrupted: false,
        },
    );
    assert_eq!(read_file_range(&store, &root, 0, 99).unwrap(), bytes);
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::Other.into())
        }
    }
    let stage = store.begin_staging(Duration::from_secs(3600)).unwrap();
    let staged = StagedContentStore::new(&store, &stage.id).unwrap();
    assert!(import_file(&staged, &mut Broken).is_err());
    assert!(store.retention().unwrap().staging.contains_key(&stage.id));
    store.abandon_staging(&stage.id).unwrap();
}

#[test]
fn revoked_staging_cannot_continue_an_import_or_acquire_a_root() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let stage = store.begin_staging(Duration::from_secs(3600)).unwrap();
    let staged = StagedContentStore::new(&store, &stage.id).unwrap();
    struct Revoke<'a> {
        inner: io::Take<io::Repeat>,
        store: &'a LocalStore,
        id: &'a str,
        calls: usize,
    }
    impl Read for Revoke<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls == 3 {
                self.store.abandon_staging(self.id).unwrap();
            }
            self.inner.read(buffer)
        }
    }
    let mut reader = Revoke {
        inner: io::repeat(0x44).take(2 * CHUNK_BYTES as u64),
        store: &store,
        id: &stage.id,
        calls: 0,
    };
    assert!(import_file(&staged, &mut reader).is_err());
    assert!(store.retention().unwrap().pins.is_empty());
    assert!(store.gc(Duration::ZERO).unwrap().removed_objects > 0);
}

#[test]
fn legacy_blob_ranges_and_export_remain_valid_above_the_new_chunk_threshold() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    let bytes = vec![0x41; CHUNK_BYTES * 2 + 19];
    let legacy = store.put_blob(&bytes).unwrap();
    store.pin("legacy", &legacy).unwrap();
    assert_eq!(
        file_info(&store, &legacy).unwrap().encoding,
        FileEncoding::Blob
    );
    assert_eq!(
        read_file_range(&store, &legacy, CHUNK_BYTES as u64 - 7, 41).unwrap(),
        vec![0x41; 41]
    );
    let mut sink = HashSink::default();
    let info = export_file(&store, &legacy, &mut sink).unwrap();
    assert_eq!(info.logical_size, bytes.len() as u64);
    assert_eq!(hex::encode(sink.hash.finalize()), legacy.digest);
    assert!(sink.maximum <= CHUNK_BYTES);
}

struct Metered<'a> {
    store: &'a LocalStore,
    metadata_reads: Cell<usize>,
    blob_ranges: Cell<usize>,
    maximum_blob: Cell<u64>,
}
impl<'a> Metered<'a> {
    fn new(store: &'a LocalStore) -> Self {
        Self {
            store,
            metadata_reads: Cell::new(0),
            blob_ranges: Cell::new(0),
            maximum_blob: Cell::new(0),
        }
    }
}
impl ContentStore for Metered<'_> {
    fn put_blob(&self, _: &[u8]) -> Result<ObjectRef> {
        bail!("read-only fixture")
    }
    fn put_file_node(&self, _: &FileNode) -> Result<ObjectRef> {
        bail!("read-only fixture")
    }
    fn read_object(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        assert_eq!(reference.kind, ObjectKind::File);
        assert!(reference.size_bytes <= MAX_FILE_NODE_BYTES as u64);
        self.metadata_reads.set(self.metadata_reads.get() + 1);
        self.store.read_object(reference)
    }
    fn read_blob_range(
        &self,
        reference: &ObjectRef,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        self.blob_ranges.set(self.blob_ranges.get() + 1);
        self.maximum_blob
            .set(self.maximum_blob.get().max(reference.size_bytes));
        self.store.read_blob_range(reference, offset, length)
    }
    fn copy_blob_to(&self, reference: &ObjectRef, writer: &mut dyn Write) -> Result<()> {
        self.maximum_blob
            .set(self.maximum_blob.get().max(reference.size_bytes));
        self.store.copy_blob_to(reference, writer)
    }
}
#[derive(Default)]
struct HashSink {
    hash: Sha256,
    bytes: u64,
    maximum: usize,
}
impl Write for HashSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.hash.update(bytes);
        self.bytes += bytes.len() as u64;
        self.maximum = self.maximum.max(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn large_directory() -> tempfile::TempDir {
    let base = std::env::var_os("RUDDER_CONTENT_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let workspace_home = PathBuf::from("/workspace/home");
            let home = if workspace_home.is_dir() {
                workspace_home
            } else {
                PathBuf::from(
                    std::env::var_os("HOME").expect("owned HOME for compiler-sized fixtures"),
                )
            };
            home.join(".cache/rudder-content-tests")
        });
    assert!(base.is_absolute());
    fs::create_dir_all(&base).unwrap();
    let base = base.canonicalize().unwrap();
    assert!(
        !base.starts_with(std::env::temp_dir().canonicalize().unwrap()),
        "large content fixtures must not use the system temp filesystem"
    );
    tempfile::Builder::new()
        .prefix("compiler-sized-")
        .tempdir_in(base)
        .unwrap()
}
fn expected_byte(offset: u64) -> u8 {
    let index = offset / CHUNK_BYTES as u64;
    let within = offset % CHUNK_BYTES as u64;
    if within < 8 {
        index.to_le_bytes()[within as usize]
    } else {
        0x6d
    }
}

#[test]
fn executable_sized_stream_has_bounded_chunks_random_reads_and_verified_export() {
    const SIZE: u64 = 319_346_888;
    let directory = large_directory();
    let source = directory.path().join("codex-sized-fixture");
    let mut file = File::create(&source).unwrap();
    let mut buffer = vec![0x6d; CHUNK_BYTES];
    let mut remaining = SIZE;
    let mut chunk_index = 0_u64;
    let mut expected_hash = Sha256::new();
    while remaining > 0 {
        buffer[..8].copy_from_slice(&chunk_index.to_le_bytes());
        let count = remaining.min(CHUNK_BYTES as u64) as usize;
        file.write_all(&buffer[..count]).unwrap();
        expected_hash.update(&buffer[..count]);
        remaining -= count as u64;
        chunk_index += 1;
    }
    file.sync_all().unwrap();
    drop(file);
    drop(buffer);
    let store = LocalStore::open(directory.path().join("cas")).unwrap();
    struct Reader {
        inner: File,
        maximum: usize,
    }
    impl Read for Reader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.maximum = self.maximum.max(buffer.len());
            self.inner.read(buffer)
        }
    }
    let mut reader = Reader {
        inner: File::open(&source).unwrap(),
        maximum: 0,
    };
    let started = Instant::now();
    let root = import(&store, &mut reader);
    let import_ms = started.elapsed().as_millis();
    assert!(reader.maximum <= CHUNK_BYTES);
    let info = validate_file_tree(&store, &root).unwrap();
    assert_eq!(info.logical_size, SIZE);
    let node = decode_file_node(&store.read_object(&root).unwrap()).unwrap();
    assert_eq!(node.children.len(), 77);
    assert_eq!(node.children.last().unwrap().size_bytes, 579_784);
    assert!(node
        .children
        .iter()
        .all(|child| child.size_bytes <= CHUNK_BYTES as u64));
    let expected_digest = hex::encode(expected_hash.finalize());
    assert_ne!(root.digest, expected_digest);
    let metered = Metered::new(&store);
    for (offset, length) in [
        (0, 64),
        (CHUNK_BYTES as u64 - 9, 37),
        (CHUNK_BYTES as u64, 13),
        (2 * CHUNK_BYTES as u64 - 13, 51),
        (SIZE - 32, 99),
        (SIZE, 19),
        (SIZE + 7, 3),
    ] {
        let before = metered.blob_ranges.get();
        let bytes = read_file_range(&metered, &root, offset, length).unwrap();
        let count = SIZE.saturating_sub(offset).min(length as u64) as usize;
        assert_eq!(
            bytes,
            (0..count)
                .map(|index| expected_byte(offset + index as u64))
                .collect::<Vec<_>>()
        );
        assert!(
            metered.blob_ranges.get() - before <= 2,
            "a short random read must not scan 77 chunks"
        );
    }
    let bytes = read_file_range(&metered, &root, CHUNK_BYTES as u64 - 3, MAX_RANGE_BYTES).unwrap();
    assert_eq!(bytes.len(), MAX_RANGE_BYTES);
    assert!(metered.maximum_blob.get() <= CHUNK_BYTES as u64);
    drop(bytes);
    let mut sink = HashSink::default();
    let started = Instant::now();
    export_file(&metered, &root, &mut sink).unwrap();
    let export_ms = started.elapsed().as_millis();
    assert_eq!(sink.bytes, SIZE);
    assert_eq!(hex::encode(sink.hash.finalize()), expected_digest);
    assert!(sink.maximum <= CHUNK_BYTES);
    assert_eq!(store.closure(&root).unwrap().len(), 78);
    eprintln!(
        "content-workload {}",
        serde_json::json!({"logical_bytes":SIZE,"chunks":77,"last_chunk_bytes":579784,"descriptor_bytes":root.size_bytes,"maximum_import_read":reader.maximum,"maximum_chunk_bytes":metered.maximum_blob.get(),"maximum_export_write":sink.maximum,"import_and_publish_ms":import_ms,"export_ms":export_ms})
    );
}
