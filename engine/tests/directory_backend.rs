use anyhow::{bail, Context, Result};
use rudder_engine::{
    content::{FileNode, FILE_LAYOUT},
    directory::{
        self, decode_directory_root, decode_shard, DirectoryEntry, DirectoryShard, DirectoryStore,
        EntryNode, LocalDirectoryStore, PagedDirectory, ShardChild, ShardLink, ShardNode, Summary,
        LEAF_ENTRIES_BYTES, MAX_PAGE_BYTES,
    },
    protocol::{canonical_json, ObjectKind, ObjectRef},
    store::{DirectoryManifest, LocalStore},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

#[derive(Default)]
struct MemoryStore {
    objects: RefCell<BTreeMap<ObjectRef, Vec<u8>>>,
    reads: Cell<u64>,
    writes: Cell<u64>,
    blob_reads: Cell<u64>,
}
impl MemoryStore {
    fn insert(&self, kind: ObjectKind, bytes: &[u8]) -> ObjectRef {
        let reference = ObjectRef::from_bytes(kind, bytes);
        self.objects
            .borrow_mut()
            .insert(reference.clone(), bytes.to_vec());
        reference
    }
    fn metadata<T: serde::Serialize>(&self, kind: ObjectKind, value: &T) -> ObjectRef {
        self.insert(kind, &canonical_json(value).unwrap())
    }
    fn reset(&self) {
        self.reads.set(0);
        self.writes.set(0);
        self.blob_reads.set(0);
    }
    fn shard_refs(&self, root: &ObjectRef) -> BTreeSet<ObjectRef> {
        let root = decode_directory_root(&self.read(root).unwrap()).unwrap();
        let mut pending = vec![root.directory_shard];
        let mut seen = BTreeSet::new();
        while let Some(reference) = pending.pop() {
            if !seen.insert(reference.clone()) {
                continue;
            }
            let shard = decode_shard(&self.read(&reference).unwrap()).unwrap();
            if let ShardNode::Branch { children } = shard.node {
                pending.extend(children.into_iter().map(|c| c.shard.reference()));
            }
        }
        seen
    }
    fn rewritten_root(&self, root: &ObjectRef, shard: &DirectoryShard) -> ObjectRef {
        let mut directory = decode_directory_root(&self.read(root).unwrap()).unwrap();
        directory.directory_shard = self.metadata(ObjectKind::DirectoryShard, shard);
        directory.entry_count = shard.summary.entry_count;
        directory.subdirectory_count = shard.summary.subdirectory_count;
        self.metadata(ObjectKind::Directory, &directory)
    }
}
impl DirectoryStore for MemoryStore {
    fn read(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.reads.set(self.reads.get() + 1);
        if reference.kind == ObjectKind::Blob {
            self.blob_reads.set(self.blob_reads.get() + 1);
        }
        self.objects
            .borrow()
            .get(reference)
            .cloned()
            .context("missing test object")
    }
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        directory::references_from_bytes(kind, bytes)?;
        self.writes.set(self.writes.get() + 1);
        Ok(self.insert(kind, bytes))
    }
}
fn file(object: &ObjectRef) -> EntryNode {
    EntryNode::File {
        object: object.clone(),
        logical_size: object.size_bytes,
        mode: 0o644,
    }
}
fn entry(name: &[u8], object: &ObjectRef) -> DirectoryEntry {
    DirectoryEntry::new(name, file(object)).unwrap()
}
fn many(object: &ObjectRef, count: usize) -> Vec<DirectoryEntry> {
    (0..count)
        .map(|i| {
            entry(
                format!("source-{i:08}-with-a-stable-raw-name").as_bytes(),
                object,
            )
        })
        .collect()
}
fn root_shard(store: &MemoryStore, root: &ObjectRef) -> DirectoryShard {
    let directory = decode_directory_root(&store.read(root).unwrap()).unwrap();
    decode_shard(&store.read(&directory.directory_shard).unwrap()).unwrap()
}

#[test]
fn flat_directory_above_four_mib_has_bounded_lookup_update_and_stable_early_insert() {
    let store = MemoryStore::default();
    let blob = store.insert(ObjectKind::Blob, b"shared contents");
    let entries = many(&blob, 25_000);
    let legacy = DirectoryManifest {
        format_version: 1,
        mode: 0o755,
        entries: entries
            .iter()
            .map(|e| {
                rudder_engine::store::DirectoryEntry::new(
                    &e.name().unwrap(),
                    rudder_engine::store::EntryNode::File {
                        object: blob.clone(),
                        mode: 0o644,
                    },
                )
                .unwrap()
            })
            .collect(),
    };
    let legacy_bytes = canonical_json(&legacy).unwrap().len();
    assert!(legacy_bytes > 4 * 1024 * 1024);
    let tree = PagedDirectory::new(&store);
    let root = tree.build(0o755, entries.clone()).unwrap();
    let refs = store.shard_refs(&root);
    assert!(refs.len() > 40);
    assert!(refs.iter().all(|r| r.size_bytes <= MAX_PAGE_BYTES as u64));
    assert!(root.size_bytes < 1024);
    let mut lookup_max = 0;
    for index in [0, 12_345, 24_999] {
        store.reset();
        assert_eq!(
            tree.lookup(&root, &entries[index].name().unwrap()).unwrap(),
            Some(entries[index].clone())
        );
        assert!(store.reads.get() <= 8, "lookup reads {}", store.reads.get());
        lookup_max = lookup_max.max(store.reads.get());
        assert_eq!(store.blob_reads.get(), 0);
    }
    store.reset();
    let page = tree
        .scan(&root, Some(&entries[24_950].name().unwrap()), 17)
        .unwrap();
    assert_eq!(page.entries, entries[24_951..24_968]);
    assert!(store.reads.get() <= 10, "seek reads {}", store.reads.get());
    let seek_reads = store.reads.get();

    store.reset();
    let inserted = tree.update(&root, b"\x01early", Some(file(&blob))).unwrap();
    assert!(inserted.shards_read <= 2);
    assert!(inserted.shards_written <= 2);
    let new_refs = store.shard_refs(&inserted.root);
    assert!(
        refs.is_subset(&new_refs),
        "early insertion repartitioned old pages"
    );
    assert_eq!(tree.read_root(&inserted.root).unwrap().entry_count, 25_001);
    let removed = tree.update(&inserted.root, b"\x01early", None).unwrap();
    assert_eq!(
        removed.root, root,
        "deletion did not restore canonical identity"
    );
    let inside = tree
        .update(&root, b"source-00000000-before", Some(file(&blob)))
        .unwrap();
    assert!(inside.shards_read <= 8 && inside.shards_written <= 8);
    let inside_refs = store.shard_refs(&inside.root);
    assert!(
        refs.difference(&inside_refs).count() <= 8,
        "early insertion shifted unrelated existing-prefix pages"
    );
    assert_eq!(
        tree.update(&inside.root, b"source-00000000-before", None)
            .unwrap()
            .root,
        root
    );

    let replacement = store.insert(ObjectKind::Blob, b"changed");
    let updated = tree
        .update(&root, &entries[0].name().unwrap(), Some(file(&replacement)))
        .unwrap();
    assert!(updated.shards_read <= 8 && updated.shards_written <= 8);
    let changed_refs = store.shard_refs(&updated.root);
    assert!(refs.difference(&changed_refs).count() <= 8);
    assert_eq!(
        tree.lookup(&root, &entries[0].name().unwrap()).unwrap(),
        Some(entries[0].clone())
    );
    assert_eq!(
        tree.lookup(&updated.root, &entries[0].name().unwrap())
            .unwrap()
            .unwrap()
            .node,
        file(&replacement)
    );
    let validation = tree.validate_directory(&updated.root, 4096).unwrap();
    assert_eq!(validation.summary.entry_count, 25_000);
    assert_eq!(validation.summary.subdirectory_count, 0);
    assert_eq!(
        validation.references,
        vec![blob, replacement]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    println!(
        "{}",
        serde_json::json!({"entries":25_000,"legacy_bytes":legacy_bytes,"root_bytes":root.size_bytes,"shards":refs.len(),"max_shard_bytes":refs.iter().map(|r|r.size_bytes).max(),"lookup_max_object_reads":lookup_max,"seek_object_reads":seek_reads,"early_insert_shard_reads":inserted.shards_read,"early_insert_shard_writes":inserted.shards_written,"replacement_shard_reads":updated.shards_read,"replacement_shard_writes":updated.shards_written})
    );
}

#[test]
fn terminators_compressed_prefixes_raw_names_and_paginated_order_are_exact() {
    let store = MemoryStore::default();
    let blob = store.insert(ObjectKind::Blob, b"x");
    let tree = PagedDirectory::new(&store);
    let child = tree.build(0o700, Vec::new()).unwrap();
    let mut entries: Vec<_> = (0..2500)
        .map(|i| entry(format!("aa{i:08}").as_bytes(), &blob))
        .collect();
    entries.extend([
        entry(b"a", &blob),
        entry(b"aa", &blob),
        entry(b"a\x80", &blob),
        entry(b"a\xff", &blob),
    ]);
    entries.push(
        DirectoryEntry::new(
            b"a-dir",
            EntryNode::Directory {
                object: child.clone(),
            },
        )
        .unwrap(),
    );
    entries.push(
        DirectoryEntry::new(
            b"a-link",
            EntryNode::Symlink {
                target_hex: hex::encode(b"../\xff"),
            },
        )
        .unwrap(),
    );
    entries.sort_by(|a, b| a.name_hex.cmp(&b.name_hex));
    let root = tree.build(0o2755, entries.clone()).unwrap();
    let shard = root_shard(&store, &root);
    assert_eq!(shard.prefix_hex, hex::encode(b"a"));
    let ShardNode::Branch { children } = shard.node else {
        panic!("expected branch")
    };
    assert_eq!(children[0].edge, 0);
    assert_eq!(
        tree.lookup(&root, b"a").unwrap().unwrap().name().unwrap(),
        b"a"
    );
    assert_eq!(
        tree.lookup(&root, b"aa").unwrap().unwrap().name().unwrap(),
        b"aa"
    );
    assert!(tree.lookup(&root, b"ab").unwrap().is_none());
    assert!(tree.lookup(&root, b"aaz").unwrap().is_none());
    assert_eq!(tree.read_root(&root).unwrap().subdirectory_count, 1);
    assert_eq!(tree.read_root(&root).unwrap().mode, 0o2755);
    let mut after = None;
    let mut scanned = Vec::new();
    loop {
        let page = tree.scan(&root, after.as_deref(), 73).unwrap();
        scanned.extend(page.entries);
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(scanned, entries);
    let validation = tree.validate_directory(&root, 1000).unwrap();
    assert_eq!(validation.summary.subdirectory_count, 1);
    assert!(validation.references.contains(&child));
    assert!(tree.scan(&root, None, 0).is_err());
    assert!(tree.scan(&root, None, 1025).is_err());
    assert!(tree.scan(&root, Some(b"../bad"), 1).is_err());
}

#[test]
fn updates_coalesce_canonically_and_path_copy_preserves_parent_siblings() {
    let store = MemoryStore::default();
    let blob = store.insert(ObjectKind::Blob, b"base");
    let other = store.insert(ObjectKind::Blob, b"other");
    let tree = PagedDirectory::new(&store);
    let mut entries = many(&blob, 320);
    let large = tree.build(0o751, entries.clone()).unwrap();
    assert!(matches!(
        root_shard(&store, &large).node,
        ShardNode::Branch { .. }
    ));
    let mut current = large.clone();
    for deleted in entries.drain(190..) {
        current = tree
            .update(&current, &deleted.name().unwrap(), None)
            .unwrap()
            .root;
    }
    assert!(matches!(
        root_shard(&store, &current).node,
        ShardNode::Leaf { .. }
    ));
    assert_eq!(
        current,
        tree.build(0o751, entries.iter().rev().cloned()).unwrap()
    );
    let parent = tree
        .build(
            0o755,
            [
                DirectoryEntry::new(
                    b"left",
                    EntryNode::Directory {
                        object: current.clone(),
                    },
                )
                .unwrap(),
                DirectoryEntry::new(
                    b"right",
                    EntryNode::Directory {
                        object: current.clone(),
                    },
                )
                .unwrap(),
            ],
        )
        .unwrap();
    let changed = tree
        .update_path(
            &parent,
            &[b"left".to_vec(), entries[0].name().unwrap()],
            Some(file(&other)),
        )
        .unwrap();
    let left = tree.lookup(&changed.root, b"left").unwrap().unwrap();
    let EntryNode::Directory { object: left } = left.node else {
        panic!()
    };
    assert_ne!(left, current);
    assert_eq!(
        tree.lookup(&changed.root, b"right").unwrap().unwrap().node,
        EntryNode::Directory {
            object: current.clone()
        }
    );
    assert_eq!(
        tree.lookup(&parent, b"left").unwrap().unwrap().node,
        EntryNode::Directory {
            object: current.clone()
        }
    );
    assert_eq!(
        tree.lookup(&current, &entries[0].name().unwrap())
            .unwrap()
            .unwrap()
            .node,
        file(&blob)
    );
    assert_eq!(tree.read_root(&left).unwrap().mode, 0o751);
    assert!(tree
        .update_path(&parent, &[b"missing".to_vec(), b"x".to_vec()], None)
        .is_err());
    assert!(tree
        .update_path(&left, &[entries[0].name().unwrap(), b"x".to_vec()], None)
        .is_err());
    assert!(tree.update_path(&parent, &[b"..".to_vec()], None).is_err());
    assert_eq!(tree.update(&parent, b"missing", None).unwrap().root, parent);
    let remoded = tree.with_mode(&parent, 0o700).unwrap();
    assert_eq!(
        tree.read_root(&parent).unwrap().directory_shard,
        tree.read_root(&remoded).unwrap().directory_shard
    );
}

#[test]
fn file_descriptor_length_is_not_file_length_and_child_semantics_are_checked() {
    let store = MemoryStore::default();
    let blob = store.insert(ObjectKind::Blob, b"file bytes");
    let file_node = FileNode {
        format_version: 1,
        layout: FILE_LAYOUT.into(),
        level: 0,
        logical_size: blob.size_bytes,
        children: vec![blob.clone()],
    };
    let bytes = canonical_json(&file_node).unwrap();
    let descriptor = store.insert(ObjectKind::File, &bytes);
    assert_ne!(descriptor.size_bytes, blob.size_bytes);
    let tree = PagedDirectory::new(&store);
    let root = tree
        .build(
            0o755,
            [DirectoryEntry::new(
                b"chunked",
                EntryNode::File {
                    object: descriptor.clone(),
                    logical_size: blob.size_bytes,
                    mode: 0o4755,
                },
            )
            .unwrap()],
        )
        .unwrap();
    let shard = root_shard(&store, &root);
    directory::validate_child(
        ObjectKind::DirectoryShard,
        &canonical_json(&shard).unwrap(),
        &descriptor,
        &bytes,
    )
    .unwrap();
    let wrong = tree
        .build(
            0o755,
            [DirectoryEntry::new(
                b"chunked",
                EntryNode::File {
                    object: descriptor.clone(),
                    logical_size: descriptor.size_bytes,
                    mode: 0o644,
                },
            )
            .unwrap()],
        )
        .unwrap();
    let shard = root_shard(&store, &wrong);
    assert!(directory::validate_child(
        ObjectKind::DirectoryShard,
        &canonical_json(&shard).unwrap(),
        &descriptor,
        &bytes
    )
    .is_err());
    assert!(DirectoryEntry::new(
        b"bad",
        EntryNode::File {
            object: blob.clone(),
            logical_size: blob.size_bytes + 1,
            mode: 0o644
        }
    )
    .is_err());
    assert!(DirectoryEntry::new(
        b"bad",
        EntryNode::File {
            object: blob.clone(),
            logical_size: blob.size_bytes,
            mode: 0o100644
        }
    )
    .is_err());
    assert!(DirectoryEntry::new(
        b"bad",
        EntryNode::File {
            object: root,
            logical_size: 1,
            mode: 0o644
        }
    )
    .is_err());
}

#[test]
fn swapped_pages_forged_summaries_and_wrong_compressed_prefixes_fail_closed() {
    let store = MemoryStore::default();
    let blob = store.insert(ObjectKind::Blob, b"x");
    let tree = PagedDirectory::new(&store);
    let root = tree.build(0o755, many(&blob, 2000)).unwrap();
    let mut swapped = root_shard(&store, &root);
    let ShardNode::Branch { children } = &mut swapped.node else {
        panic!()
    };
    let a = children[0].shard.clone();
    children[0].shard = children[1].shard.clone();
    children[1].shard = a;
    let swapped = store.rewritten_root(&root, &swapped);
    assert!(tree.validate_directory(&swapped, 1000).is_err());
    assert!(tree
        .lookup(&swapped, &many(&blob, 1)[0].name().unwrap())
        .is_err());

    let mut forged = root_shard(&store, &root);
    let ShardNode::Branch { children } = &mut forged.node else {
        panic!()
    };
    children[0].shard.summary.entry_count += 1;
    forged.summary.entry_count += 1;
    let forged = store.rewritten_root(&root, &forged);
    assert!(tree.validate_directory(&forged, 1000).is_err());
    assert!(tree
        .update(&forged, &many(&blob, 1)[0].name().unwrap(), None)
        .is_err());

    let mut root_meta = tree.read_root(&root).unwrap();
    root_meta.entry_count += 1;
    let forged = store.metadata(ObjectKind::Directory, &root_meta);
    assert!(tree.lookup(&forged, b"anything").is_err());

    let mut prefix = root_shard(&store, &root);
    prefix.prefix_hex = hex::encode(b"wrong-prefix");
    let prefix = store.rewritten_root(&root, &prefix);
    assert!(tree.validate_directory(&prefix, 1000).is_err());
    assert!(tree.validate_directory(&root, 1).is_err());
}

#[test]
fn malformed_local_shapes_names_counts_order_and_raw_hashes_are_rejected() {
    let store = MemoryStore::default();
    let blob = store.insert(ObjectKind::Blob, b"x");
    for name in [b"".as_slice(), b".", b"..", b"a/b", b"a\0b", &[b'x'; 256]] {
        assert!(DirectoryEntry::new(name, file(&blob)).is_err());
    }
    assert!(DirectoryEntry::new(
        b"x",
        EntryNode::Symlink {
            target_hex: "00".into()
        }
    )
    .is_err());
    assert!(DirectoryEntry::new(
        b"x",
        EntryNode::Symlink {
            target_hex: hex::encode(vec![b'x'; 4097])
        }
    )
    .is_err());
    let tree = PagedDirectory::new(&store);
    assert!(tree
        .build(0o755, [entry(b"a", &blob), entry(b"a", &blob)])
        .is_err());
    let root = tree
        .build(0o755, [entry(b"a", &blob), entry(b"b", &blob)])
        .unwrap();
    let shard = root_shard(&store, &root);
    let bytes = canonical_json(&shard).unwrap();
    let mut noncanonical = bytes.clone();
    noncanonical.push(b'\n');
    assert!(decode_shard(&noncanonical).is_err());
    let mut value = serde_json::to_value(&shard).unwrap();
    value["node"]["entries"][0]["name_hex"] = "6A".into();
    assert!(decode_shard(&canonical_json(&value).unwrap()).is_err());
    value = serde_json::to_value(&shard).unwrap();
    value["node"]["entries"].as_array_mut().unwrap().swap(0, 1);
    assert!(decode_shard(&canonical_json(&value).unwrap()).is_err());
    value = serde_json::to_value(&shard).unwrap();
    value["surprise"] = true.into();
    assert!(decode_shard(&canonical_json(&value).unwrap()).is_err());

    let reference = tree.read_root(&root).unwrap().directory_shard;
    store
        .objects
        .borrow_mut()
        .get_mut(&reference)
        .unwrap()
        .push(b' ');
    assert!(tree.lookup(&root, b"a").is_err());
    let children = [98, 99]
        .into_iter()
        .map(|edge| ShardChild {
            edge,
            shard: ShardLink {
                digest: "a".repeat(64 - usize::from(edge == 99))
                    + if edge == 99 { "b" } else { "" },
                size_bytes: 500,
                summary: Summary {
                    entry_count: u64::MAX / 2 + 1,
                    subdirectory_count: 0,
                    entries_bytes: u64::MAX / 2 + 1,
                },
            },
        })
        .collect();
    let overflow = DirectoryShard {
        format_version: 1,
        prefix_hex: String::new(),
        summary: Summary {
            entry_count: u64::MAX,
            subdirectory_count: 0,
            entries_bytes: u64::MAX,
        },
        node: ShardNode::Branch { children },
    };
    assert!(decode_shard(&canonical_json(&overflow).unwrap()).is_err());
    let mut too_big = tree.read_root(&root).unwrap();
    too_big.directory_shard.size_bytes = MAX_PAGE_BYTES as u64 + 1;
    assert!(decode_directory_root(&canonical_json(&too_big).unwrap()).is_err());
    assert!(LEAF_ENTRIES_BYTES < MAX_PAGE_BYTES as u64);
}

#[test]
fn local_adapter_requires_live_staging_and_legacy_directory_stays_readable() {
    let base = std::env::var_os("RUDDER_TEST_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            if std::path::Path::new("/workspace").is_dir() {
                "/workspace".into()
            } else {
                std::env::temp_dir()
            }
        });
    let temp = tempfile::Builder::new()
        .prefix("rudder-directory-")
        .tempdir_in(base)
        .unwrap();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let blob = store.put_blob(b"file").unwrap();
    let legacy = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![rudder_engine::store::DirectoryEntry::new(
                b"file",
                rudder_engine::store::EntryNode::File {
                    object: blob.clone(),
                    mode: 0o644,
                },
            )
            .unwrap()],
        })
        .unwrap();
    let lease = store.begin_staging(Duration::from_secs(60)).unwrap();
    let adapter = LocalDirectoryStore::new(&store, &lease.id);
    let tree = PagedDirectory::new(&adapter);
    let root = tree.build(0o755, [entry(b"file", &blob)]).unwrap();
    store.publish(&lease.id, "directory-test", &root).unwrap();
    assert!(store
        .closure(&root)
        .unwrap()
        .iter()
        .any(|r| r.kind == ObjectKind::DirectoryShard));
    assert_eq!(store.read_directory(&legacy).unwrap().entries.len(), 1);
    assert!(tree.read_root(&legacy).is_err());
    let read = LocalDirectoryStore::read_only(&store);
    let tree = PagedDirectory::new(&read);
    assert_eq!(
        tree.lookup(&root, b"file").unwrap().unwrap().node,
        file(&blob)
    );
    assert!(tree.update(&root, b"new", Some(file(&blob))).is_err());
    store.abandon_staging(&lease.id).unwrap();
    assert!(PagedDirectory::new(&adapter)
        .update(&root, b"new", Some(file(&blob)))
        .is_err());
    drop(store);
    let reopened = LocalStore::open(temp.path().join("cas")).unwrap();
    let read = LocalDirectoryStore::read_only(&reopened);
    assert_eq!(
        PagedDirectory::new(&read)
            .lookup(&root, b"file")
            .unwrap()
            .unwrap()
            .node,
        file(&blob)
    );
}

#[test]
fn provider_write_failure_or_false_ack_cannot_return_a_new_root() {
    struct Fails;
    impl DirectoryStore for Fails {
        fn read(&self, _: &ObjectRef) -> Result<Vec<u8>> {
            bail!("unavailable")
        }
        fn put(&self, _: ObjectKind, _: &[u8]) -> Result<ObjectRef> {
            bail!("provider failed")
        }
    }
    assert!(PagedDirectory::new(&Fails).build(0o755, []).is_err());
    struct Lies;
    impl DirectoryStore for Lies {
        fn read(&self, _: &ObjectRef) -> Result<Vec<u8>> {
            bail!("unavailable")
        }
        fn put(&self, _: ObjectKind, _: &[u8]) -> Result<ObjectRef> {
            Ok(ObjectRef::from_bytes(ObjectKind::Blob, b"wrong"))
        }
    }
    assert!(PagedDirectory::new(&Lies).build(0o755, []).is_err());
    struct RootFails(MemoryStore);
    impl DirectoryStore for RootFails {
        fn read(&self, object: &ObjectRef) -> Result<Vec<u8>> {
            self.0.read(object)
        }
        fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
            if kind == ObjectKind::Directory {
                bail!("root publication write failed");
            }
            self.0.put(kind, bytes)
        }
    }
    let partial = RootFails(MemoryStore::default());
    assert!(PagedDirectory::new(&partial).build(0o755, []).is_err());
    assert!(partial
        .0
        .objects
        .borrow()
        .keys()
        .all(|r| r.kind == ObjectKind::DirectoryShard));
    assert_eq!(partial.0.writes.get(), 1);
}

#[test]
fn exact_terminator_binding_and_worst_fanout_wire_bound() {
    let store = MemoryStore::default();
    let blob = store.insert(ObjectKind::Blob, b"x");
    let tree = PagedDirectory::new(&store);
    let mut entries: Vec<_> = (0..1000)
        .map(|i| entry(format!("aa{i:08}").as_bytes(), &blob))
        .collect();
    entries.push(entry(b"a", &blob));
    let root = tree.build(0o755, entries).unwrap();
    let alternate = tree.build(0o755, [entry(b"aa", &blob)]).unwrap();
    let alternate_ref = tree.read_root(&alternate).unwrap().directory_shard;
    let alternate_page = root_shard(&store, &alternate);
    let mut parent = root_shard(&store, &root);
    let ShardNode::Branch { children } = &mut parent.node else {
        panic!()
    };
    assert_eq!(children[0].edge, 0);
    let old = children[0].shard.summary.entries_bytes;
    children[0].shard = ShardLink {
        digest: alternate_ref.digest.clone(),
        size_bytes: alternate_ref.size_bytes,
        summary: alternate_page.summary,
    };
    parent.summary.entries_bytes += alternate_page.summary.entries_bytes - old;
    let bytes = canonical_json(&parent).unwrap();
    decode_shard(&bytes).unwrap(); // local refs can arrive before their children
    assert!(directory::validate_child(
        ObjectKind::DirectoryShard,
        &bytes,
        &alternate_ref,
        &canonical_json(&alternate_page).unwrap()
    )
    .is_err());

    let mut summary = Summary::default();
    let mut children = Vec::new();
    for edge in (0..=256u16).filter(|edge| !matches!(edge, 1 | 48)) {
        let count = if edge == 0 { 1 } else { u64::MAX / 256 };
        let child_summary = Summary {
            entry_count: count,
            subdirectory_count: count,
            entries_bytes: count,
        };
        summary.entry_count = summary.entry_count.checked_add(count).unwrap();
        summary.subdirectory_count = summary.subdirectory_count.checked_add(count).unwrap();
        summary.entries_bytes = summary.entries_bytes.checked_add(count).unwrap();
        let object = ObjectRef::from_bytes(ObjectKind::DirectoryShard, &edge.to_be_bytes());
        children.push(ShardChild {
            edge,
            shard: ShardLink {
                digest: object.digest,
                size_bytes: MAX_PAGE_BYTES as u64,
                summary: child_summary,
            },
        });
    }
    let branch = DirectoryShard {
        format_version: 1,
        prefix_hex: String::new(),
        summary,
        node: ShardNode::Branch { children },
    };
    let bytes = canonical_json(&branch).unwrap();
    assert!(
        bytes.len() <= MAX_PAGE_BYTES,
        "worst fanout {}",
        bytes.len()
    );
    decode_shard(&bytes).unwrap();
}

#[test]
fn mixed_mutation_history_converges_to_the_same_canonical_root() {
    let store = MemoryStore::default();
    let blob = store.insert(ObjectKind::Blob, b"base");
    let alternate = store.insert(ObjectKind::Blob, b"alternate");
    let tree = PagedDirectory::new(&store);
    let mut model: BTreeMap<Vec<u8>, EntryNode> = (0..400)
        .map(|i| (format!("same-prefix-{i:04x}").into_bytes(), file(&blob)))
        .collect();
    let mut root = tree
        .build(
            0o755,
            model
                .iter()
                .map(|(name, node)| DirectoryEntry::new(name, node.clone()).unwrap()),
        )
        .unwrap();
    let mut random = 0x1ed5_u64;
    for i in 0..120 {
        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
        let name = format!("same-prefix-{:04x}", (random >> 17) % 700).into_bytes();
        let node = if random & 3 == 0 {
            None
        } else {
            Some(file(if random & 4 == 0 { &blob } else { &alternate }))
        };
        match &node {
            Some(node) => {
                model.insert(name.clone(), node.clone());
            }
            None => {
                model.remove(&name);
            }
        }
        root = tree.update(&root, &name, node).unwrap().root;
        if i % 20 == 19 {
            let rebuilt = tree
                .build(
                    0o755,
                    model
                        .iter()
                        .rev()
                        .map(|(name, node)| DirectoryEntry::new(name, node.clone()).unwrap()),
                )
                .unwrap();
            assert_eq!(root, rebuilt, "history-dependent layout after mutation {i}");
        }
    }
}

#[test]
fn publication_checks_actual_child_counts_and_file_logical_size() {
    let base = std::env::var_os("RUDDER_TEST_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("rudder-directory-publication-")
        .tempdir_in(base)
        .unwrap();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let lease = store.begin_staging(Duration::from_secs(60)).unwrap();
    let blob = store.put_blob(b"file bytes").unwrap();
    let node = FileNode {
        format_version: 1,
        layout: FILE_LAYOUT.into(),
        level: 0,
        logical_size: blob.size_bytes,
        children: vec![blob.clone()],
    };
    let descriptor = store.put_metadata(ObjectKind::File, &node).unwrap();
    let adapter = LocalDirectoryStore::new(&store, &lease.id);
    let tree = PagedDirectory::new(&adapter);
    let good = tree
        .build(
            0o755,
            [DirectoryEntry::new(
                b"file",
                EntryNode::File {
                    object: descriptor.clone(),
                    logical_size: blob.size_bytes,
                    mode: 0o644,
                },
            )
            .unwrap()],
        )
        .unwrap();
    let mut counts = tree.read_root(&good).unwrap();
    counts.entry_count += 1;
    let bytes = canonical_json(&counts).unwrap();
    let forged = ObjectRef::from_bytes(ObjectKind::Directory, &bytes);
    store.receive_staged(&lease.id, &forged, &bytes).unwrap(); // receive order is independent
    assert!(
        store
            .publish(&lease.id, "bad-directory-counts", &forged)
            .is_err(),
        "publication trusted a root's claimed count instead of its actual child"
    );
    assert!(!store
        .retention()
        .unwrap()
        .pins
        .contains_key("bad-directory-counts"));

    let wrong_length = tree
        .build(
            0o755,
            [DirectoryEntry::new(
                b"file",
                EntryNode::File {
                    object: descriptor.clone(),
                    logical_size: descriptor.size_bytes,
                    mode: 0o644,
                },
            )
            .unwrap()],
        )
        .unwrap();
    assert!(
        store
            .publish(&lease.id, "bad-file-logical-size", &wrong_length)
            .is_err(),
        "publication treated descriptor bytes as logical file length"
    );
    assert!(!store
        .retention()
        .unwrap()
        .pins
        .contains_key("bad-file-logical-size"));
    store.publish(&lease.id, "good-directory", &good).unwrap();
    let next = store.begin_staging(Duration::from_secs(60)).unwrap();
    assert!(
        store
            .publish_extending(&next.id, "bad-directory-retained-child", &forged, &[good])
            .is_err(),
        "retained child bypassed a new parent's semantic edge check"
    );
    store.abandon_staging(&next.id).unwrap();
}

#[test]
fn branch_reference_aliases_are_rejected_but_every_repeated_file_length_is_checked() {
    let store = MemoryStore::default();
    let blob = store.insert(ObjectKind::Blob, b"data");
    let tree = PagedDirectory::new(&store);
    let root = tree.build(0o755, many(&blob, 400)).unwrap();
    let mut branch = root_shard(&store, &root);
    let ShardNode::Branch { children } = &mut branch.node else {
        panic!()
    };
    let first = children[0].shard.clone();
    let old_summary = children[1].shard.summary;
    children[1].shard = first.clone();
    branch.summary.entry_count =
        branch.summary.entry_count - old_summary.entry_count + first.summary.entry_count;
    branch.summary.subdirectory_count = branch.summary.subdirectory_count
        - old_summary.subdirectory_count
        + first.summary.subdirectory_count;
    branch.summary.entries_bytes =
        branch.summary.entries_bytes - old_summary.entries_bytes + first.summary.entries_bytes;
    let bytes = canonical_json(&branch).unwrap();
    assert!(decode_shard(&bytes)
        .unwrap_err()
        .to_string()
        .contains("branch repeats a shard"));
    assert!(directory::validate_child(
        ObjectKind::DirectoryShard,
        &bytes,
        &first.reference(),
        &store.read(&first.reference()).unwrap()
    )
    .is_err());

    let node = FileNode {
        format_version: 1,
        layout: FILE_LAYOUT.into(),
        level: 0,
        logical_size: blob.size_bytes,
        children: vec![blob.clone()],
    };
    let descriptor_bytes = canonical_json(&node).unwrap();
    let descriptor = store.insert(ObjectKind::File, &descriptor_bytes);
    let make = |logical_size| EntryNode::File {
        object: descriptor.clone(),
        logical_size,
        mode: 0o644,
    };
    let good = tree
        .build(
            0o755,
            [
                DirectoryEntry::new(b"a", make(blob.size_bytes)).unwrap(),
                DirectoryEntry::new(b"b", make(blob.size_bytes)).unwrap(),
            ],
        )
        .unwrap();
    directory::validate_child(
        ObjectKind::DirectoryShard,
        &canonical_json(&root_shard(&store, &good)).unwrap(),
        &descriptor,
        &descriptor_bytes,
    )
    .unwrap();
    let wrong = tree
        .build(
            0o755,
            [
                DirectoryEntry::new(b"a", make(blob.size_bytes)).unwrap(),
                DirectoryEntry::new(b"b", make(blob.size_bytes + 1)).unwrap(),
            ],
        )
        .unwrap();
    assert!(
        directory::validate_child(
            ObjectKind::DirectoryShard,
            &canonical_json(&root_shard(&store, &wrong)).unwrap(),
            &descriptor,
            &descriptor_bytes
        )
        .is_err(),
        "a valid first alias hid a later wrong logical length"
    );
}
