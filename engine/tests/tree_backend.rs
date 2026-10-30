#![cfg(unix)]

use rudder_engine::{
    content::{self, CHUNK_BYTES},
    directory::{DirectoryEntry, EntryNode, LocalDirectoryStore, PagedDirectory},
    protocol::{canonical_json, ObjectKind, ObjectRef},
    store::{self, DirectoryManifest, ImportPolicy, LocalStore, ReferencedMetadata},
    tree::{self, HardlinkGroup, HardlinkTopology, ImportBudget, TreeSnapshot},
};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File},
    io::{Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{symlink, MetadataExt, PermissionsExt},
    },
    path::Path,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn scratch() -> TempDir {
    // Streamed fixtures must not consume Cloud's small /tmp/root volume.
    let base = Path::new("/workspace/home/.cache/rudder-tree-tests");
    if Path::new("/workspace").is_dir() {
        fs::create_dir_all(base).unwrap();
        tempfile::Builder::new()
            .prefix("tree-")
            .tempdir_in(base)
            .unwrap()
    } else {
        tempfile::tempdir().unwrap()
    }
}
fn legacy(store: &LocalStore, mode: u32, entries: Vec<store::DirectoryEntry>) -> ObjectRef {
    store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode,
            entries,
        })
        .unwrap()
}
fn old_file(name: &[u8], object: &ObjectRef) -> store::DirectoryEntry {
    store::DirectoryEntry::new(
        name,
        store::EntryNode::File {
            object: object.clone(),
            mode: 0o640,
        },
    )
    .unwrap()
}
fn file(name: &[u8], object: &ObjectRef, mode: u32) -> DirectoryEntry {
    DirectoryEntry::new(
        name,
        EntryNode::File {
            object: object.clone(),
            logical_size: object.size_bytes,
            mode,
        },
    )
    .unwrap()
}
fn paged(store: &LocalStore, stage: &str, mode: u32, entries: Vec<DirectoryEntry>) -> ObjectRef {
    PagedDirectory::new(&LocalDirectoryStore::new(store, stage))
        .build(mode, entries)
        .unwrap()
}
fn snapshot(
    store: &LocalStore,
    directory: ObjectRef,
    groups: Vec<Vec<Vec<&[u8]>>>,
) -> TreeSnapshot {
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
    TreeSnapshot {
        format_version: 1,
        directory,
        hardlinks: store.put_blob(&canonical_json(&topology).unwrap()).unwrap(),
    }
}
fn digest(path: &Path) -> String {
    let mut hash = Sha256::new();
    let mut reader = File::open(path).unwrap();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes = reader.read(&mut buffer).unwrap();
        if bytes == 0 {
            break;
        }
        hash.update(&buffer[..bytes]);
    }
    hex::encode(hash.finalize())
}
fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}
fn assert_error(error: anyhow::Error, expected: &str) {
    let message = format!("{error:#}");
    assert!(
        message.contains(expected),
        "wanted {expected:?}, got {message}"
    );
}

#[test]
fn mixed_directory_formats_have_identical_raw_order_cursors_and_shallow_upgrade() {
    let temp = scratch();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let blob = store.put_blob(b"payload").unwrap();
    let child = legacy(&store, 0o750, vec![old_file(b"child", &blob)]);
    let names: Vec<&[u8]> = vec![b"a", b"ab", b"z", &[0xff]];
    let mut entries = names
        .iter()
        .map(|name| old_file(name, &blob))
        .collect::<Vec<_>>();
    entries.push(
        store::DirectoryEntry::new(
            b"nested",
            store::EntryNode::Directory {
                object: child.clone(),
            },
        )
        .unwrap(),
    );
    let root = legacy(&store, 0o751, entries);
    let upgraded = tree::to_paged_directory(&store, &stage.id, &root).unwrap();
    let info = tree::root_info(&store, &root).unwrap();
    assert_eq!(
        (
            info.format_version,
            info.mode,
            info.entry_count,
            info.subdirectory_count
        ),
        (1, 0o751, 5, 1)
    );
    let updated_info = tree::root_info(&store, &upgraded).unwrap();
    assert_eq!(
        (
            updated_info.format_version,
            updated_info.mode,
            updated_info.entry_count,
            updated_info.subdirectory_count
        ),
        (2, 0o751, 5, 1)
    );
    assert_eq!(
        tree::lookup(&store, &upgraded, b"nested")
            .unwrap()
            .unwrap()
            .node,
        EntryNode::Directory {
            object: child.clone()
        }
    );
    assert_eq!(tree::root_info(&store, &child).unwrap().format_version, 1);
    let mut all = Vec::new();
    let mut after = None;
    loop {
        let old = tree::scan(&store, &root, after.as_deref(), 2).unwrap();
        let new = tree::scan(&store, &upgraded, after.as_deref(), 2).unwrap();
        assert_eq!(old, new);
        all.extend(old.entries.into_iter().map(|entry| entry.name().unwrap()));
        after = old.next_after;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(
        all,
        vec![
            b"a".to_vec(),
            b"ab".to_vec(),
            b"nested".to_vec(),
            b"z".to_vec(),
            vec![0xff]
        ]
    );
    assert_eq!(
        tree::lookup(&store, &root, &[0xff]).unwrap(),
        tree::lookup(&store, &upgraded, &[0xff]).unwrap()
    );
    assert!(tree::lookup(&store, &root, b"absent").unwrap().is_none());
    for invalid in [b"".as_slice(), b".", b"..", b"a/b", b"x\0y"] {
        assert!(tree::lookup(&store, &root, invalid).is_err());
    }
    assert!(tree::scan(&store, &root, None, 0).is_err());
    assert!(tree::scan(&store, &upgraded, None, 1025).is_err());
    store.abandon_staging(&stage.id).unwrap();
    assert!(tree::to_paged_directory(&store, &stage.id, &root).is_err());
    assert_eq!(
        tree::to_paged_directory(&store, &stage.id, &upgraded).unwrap(),
        upgraded,
        "existing paged roots require no writes"
    );
}

#[test]
fn streamed_chunked_files_round_trip_with_real_hardlinks_independent_copies_and_modes() {
    let temp = scratch();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("sub")).unwrap();
    fs::create_dir(source.join(".git")).unwrap();
    fs::write(source.join(".git/config"), b"included by default").unwrap();
    fs::write(source.join("small"), b"small blob").unwrap();
    let source_file = source.join("large");
    let mut output = File::create(&source_file).unwrap();
    let buffer: Vec<_> = (0..64 * 1024).map(|index| (index % 251) as u8).collect();
    let length = CHUNK_BYTES * 3 + 17;
    let mut remaining = length;
    while remaining > 0 {
        let bytes = remaining.min(buffer.len());
        output.write_all(&buffer[..bytes]).unwrap();
        remaining -= bytes;
    }
    output.sync_all().unwrap();
    drop(output);
    fs::set_permissions(&source_file, fs::Permissions::from_mode(0o640)).unwrap();
    fs::hard_link(&source_file, source.join("sub/linked")).unwrap();
    fs::copy(&source_file, source.join("independent")).unwrap();
    fs::set_permissions(
        source.join("independent"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    symlink("../large", source.join("sub/relative-link")).unwrap();
    let outside = temp.path().join("outside");
    fs::write(&outside, b"outside sentinel").unwrap();
    symlink(&outside, source.join("absolute-link")).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();
    fs::set_permissions(source.join("sub"), fs::Permissions::from_mode(0o750)).unwrap();

    let store_path = temp.path().join("cas");
    let store = LocalStore::open(&store_path).unwrap();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let started = Instant::now();
    let snapshot = tree::import_quiesced(
        &store,
        &stage.id,
        &source,
        &ImportPolicy::default(),
        &ImportBudget::default(),
    )
    .unwrap();
    assert_eq!(
        tree::root_info(&store, &snapshot.directory)
            .unwrap()
            .format_version,
        2
    );
    let object = match tree::lookup(&store, &snapshot.directory, b"large")
        .unwrap()
        .unwrap()
        .node
    {
        EntryNode::File {
            object,
            logical_size,
            mode,
        } => {
            assert_eq!((logical_size, mode), (length as u64, 0o640));
            object
        }
        _ => panic!("not a file"),
    };
    assert_eq!(object.kind, ObjectKind::File);
    assert_eq!(
        content::read_file_range(&store, &object, CHUNK_BYTES as u64 - 3, 9).unwrap(),
        (CHUNK_BYTES - 3..CHUNK_BYTES + 6)
            .map(|index| buffer[index % buffer.len()])
            .collect::<Vec<_>>()
    );
    let topology = tree::validate_snapshot(&store, &snapshot, &ImportBudget::default()).unwrap();
    assert_eq!(topology.groups.len(), 1);
    assert_eq!(
        topology.groups[0].paths_hex,
        vec![
            vec![hex::encode(b"large")],
            vec![hex::encode(b"sub"), hex::encode(b"linked")]
        ]
    );
    assert!(
        store.retention().unwrap().pins.is_empty(),
        "import does not claim publication"
    );
    assert!(store.gc(Duration::ZERO).unwrap().blocked_by_staging);
    // One explicit envelope retains both refs. Retaining the directory alone
    // cannot promise durable hardlink identity.
    let receipt = ReferencedMetadata {
        format_version: 1,
        references: vec![snapshot.directory.clone(), snapshot.hardlinks.clone()],
        payload: serde_json::to_value(&snapshot).unwrap(),
    };
    let receipt_bytes = canonical_json(&receipt).unwrap();
    let receipt_ref = ObjectRef::from_bytes(ObjectKind::ActionResult, &receipt_bytes);
    store
        .receive_staged(&stage.id, &receipt_ref, &receipt_bytes)
        .unwrap();
    store.publish(&stage.id, "tree", &receipt_ref).unwrap();
    let import_ms = started.elapsed().as_millis();
    drop(store);
    let store = LocalStore::open(&store_path).unwrap();
    assert_eq!(store.gc(Duration::ZERO).unwrap().removed_objects, 0);
    let destination = temp.path().join("export");
    let exported = Instant::now();
    tree::export_quiesced(&store, &snapshot, &destination, &ImportBudget::default()).unwrap();
    assert_eq!(digest(&source_file), digest(&destination.join("large")));
    assert_eq!(
        fs::metadata(destination.join("large")).unwrap().ino(),
        fs::metadata(destination.join("sub/linked")).unwrap().ino()
    );
    assert_ne!(
        fs::metadata(destination.join("large")).unwrap().ino(),
        fs::metadata(destination.join("independent")).unwrap().ino()
    );
    assert_eq!(
        (
            mode(&destination),
            mode(&destination.join("sub")),
            mode(&destination.join("large"))
        ),
        (0o751, 0o750, 0o640)
    );
    assert_eq!(
        fs::read(destination.join(".git/config")).unwrap(),
        b"included by default"
    );
    assert_eq!(
        fs::read_link(destination.join("sub/relative-link")).unwrap(),
        Path::new("../large")
    );
    assert_eq!(
        fs::read_link(destination.join("absolute-link")).unwrap(),
        outside
    );
    fs::write(destination.join("small"), b"private edit").unwrap();
    assert_eq!(fs::read(source.join("small")).unwrap(), b"small blob");
    assert_eq!(
        fs::read(temp.path().join("outside")).unwrap(),
        b"outside sentinel"
    );
    eprintln!("tree streamed workload logical_path_bytes={} import_publish_ms={} export_ms={} chunk_bytes={}", length * 3 + 10 + 19, import_ms, exported.elapsed().as_millis(), CHUNK_BYTES);
}

#[test]
fn import_budget_counts_every_hardlink_and_rejects_unbounded_names_depth_and_metadata() {
    let temp = scratch();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a"), b"12345678").unwrap();
    fs::hard_link(source.join("a"), source.join("b")).unwrap();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    for (budget, expected) in [
        (
            ImportBudget {
                max_entries: 1,
                ..ImportBudget::default()
            },
            "entry count",
        ),
        (
            ImportBudget {
                max_logical_bytes: 15,
                ..ImportBudget::default()
            },
            "logical bytes",
        ),
        (
            ImportBudget {
                max_depth: 0,
                ..ImportBudget::default()
            },
            "path depth",
        ),
        (
            ImportBudget {
                max_metadata_bytes: 8,
                ..ImportBudget::default()
            },
            "metadata",
        ),
    ] {
        assert_error(
            tree::import_quiesced(
                &store,
                &stage.id,
                &source,
                &ImportPolicy::default(),
                &budget,
            )
            .unwrap_err(),
            expected,
        );
        assert!(store.retention().unwrap().pins.is_empty());
        assert!(store.retention().unwrap().staging.contains_key(&stage.id));
    }
    store.abandon_staging(&stage.id).unwrap();
    assert!(tree::import_quiesced(
        &store,
        &stage.id,
        &source,
        &ImportPolicy::default(),
        &ImportBudget::default()
    )
    .is_err());
    assert!(store.gc(Duration::ZERO).unwrap().removed_objects > 0);
}

#[test]
fn exclusions_omit_complete_subtrees_and_live_cas_requires_an_explicit_exclusion() {
    let temp = scratch();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("skip")).unwrap();
    fs::write(source.join("keep"), b"ok").unwrap();
    let special = source.join("skip/fifo");
    let c_path = std::ffi::CString::new(special.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let store = LocalStore::open(source.join("cas")).unwrap();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    assert_error(
        tree::import_quiesced(
            &store,
            &stage.id,
            &source,
            &ImportPolicy::default(),
            &ImportBudget::default(),
        )
        .unwrap_err(),
        "live CAS",
    );
    let policy = ImportPolicy {
        excluded_paths: vec![vec![b"cas".to_vec()], vec![b"skip".to_vec()]],
    };
    let snapshot = tree::import_quiesced(
        &store,
        &stage.id,
        &source,
        &policy,
        &ImportBudget::default(),
    )
    .unwrap();
    assert_eq!(
        tree::root_info(&store, &snapshot.directory)
            .unwrap()
            .entry_count,
        1
    );
    assert!(tree::lookup(&store, &snapshot.directory, b"skip")
        .unwrap()
        .is_none());
    let only_cas = ImportPolicy {
        excluded_paths: vec![vec![b"cas".to_vec()]],
    };
    assert_error(
        tree::import_quiesced(
            &store,
            &stage.id,
            &source,
            &only_cas,
            &ImportBudget::default(),
        )
        .unwrap_err(),
        "special files",
    );
    assert!(tree::import_quiesced(
        &store,
        &stage.id,
        store.root(),
        &policy,
        &ImportBudget::default()
    )
    .is_err());
    let alias = temp.path().join("alias");
    symlink(&source, &alias).unwrap();
    assert_error(
        tree::import_quiesced(&store, &stage.id, &alias, &policy, &ImportBudget::default())
            .unwrap_err(),
        "real directory",
    );
    for excluded_paths in [
        vec![vec![]],
        vec![vec![b"..".to_vec()]],
        vec![vec![b"a/b".to_vec()]],
    ] {
        assert!(tree::import_quiesced(
            &store,
            &stage.id,
            &source,
            &ImportPolicy { excluded_paths },
            &ImportBudget::default()
        )
        .is_err());
    }
}

#[test]
fn topology_rejects_duplicates_mismatched_modes_content_and_symlink_traversal_before_export() {
    let temp = scratch();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let a = store.put_blob(b"a").unwrap();
    let b = store.put_blob(b"b").unwrap();
    let root = paged(
        &store,
        &stage.id,
        0o755,
        vec![
            file(b"a", &a, 0o644),
            file(b"b", &a, 0o644),
            file(b"different", &b, 0o644),
            file(b"mode", &a, 0o600),
            DirectoryEntry::new(
                b"link",
                EntryNode::Symlink {
                    target_hex: hex::encode(b"."),
                },
            )
            .unwrap(),
        ],
    );
    for groups in [
        vec![vec![vec![b"a".as_slice()], vec![b"a".as_slice()]]],
        vec![vec![vec![b"b".as_slice()], vec![b"a".as_slice()]]],
        vec![vec![vec![b"a".as_slice()], vec![b"different".as_slice()]]],
        vec![vec![vec![b"a".as_slice()], vec![b"mode".as_slice()]]],
        vec![vec![
            vec![b"a".as_slice()],
            vec![b"link".as_slice(), b"b".as_slice()],
        ]],
        vec![
            vec![vec![b"a".as_slice()], vec![b"b".as_slice()]],
            vec![vec![b"a".as_slice()], vec![b"b".as_slice()]],
        ],
    ] {
        let snapshot = snapshot(&store, root.clone(), groups);
        let target = temp.path().join("invalid-export");
        assert!(
            tree::export_quiesced(&store, &snapshot, &target, &ImportBudget::default()).is_err()
        );
        assert!(
            !target.exists(),
            "topology failure must precede filesystem creation"
        );
    }
    let good = snapshot(&store, root, vec![vec![vec![b"a"], vec![b"b"]]]);
    assert_eq!(
        tree::validate_snapshot(&store, &good, &ImportBudget::default())
            .unwrap()
            .groups
            .len(),
        1
    );
    let noncanonical = TreeSnapshot {
        hardlinks: store
            .put_blob(b"{\"groups\":[], \"format_version\":1}")
            .unwrap(),
        ..good.clone()
    };
    assert!(tree::validate_snapshot(&store, &noncanonical, &ImportBudget::default()).is_err());
    let oversized = TreeSnapshot {
        hardlinks: ObjectRef {
            size_bytes: 64 * 1024 * 1024 + 1,
            ..good.hardlinks.clone()
        },
        ..good
    };
    assert!(tree::validate_snapshot(&store, &oversized, &ImportBudget::default()).is_err());
}

#[test]
fn export_refuses_existing_paths_and_removes_partial_work_on_failure() {
    let temp = scratch();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let a = store.put_blob(b"a").unwrap();
    let missing = ObjectRef::from_bytes(ObjectKind::Blob, b"not in this store");
    let root = legacy(
        &store,
        0o755,
        vec![old_file(b"a", &a), old_file(b"z", &missing)],
    );
    let snapshot = snapshot(&store, root, vec![]);
    let target = temp.path().join("partial");
    assert!(tree::export_quiesced(&store, &snapshot, &target, &ImportBudget::default()).is_err());
    assert!(
        !target.exists(),
        "partially exported first file/directory was not removed"
    );
    fs::create_dir(&target).unwrap();
    fs::write(target.join("sentinel"), b"preserve").unwrap();
    assert!(tree::export_quiesced(&store, &snapshot, &target, &ImportBudget::default()).is_err());
    assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"preserve");
    let alias = temp.path().join("alias");
    symlink(&target, &alias).unwrap();
    assert!(tree::export_quiesced(&store, &snapshot, &alias, &ImportBudget::default()).is_err());
    assert_eq!(fs::read_link(&alias).unwrap(), target);
    let inside_cas = store.root().join("unsafe-export");
    assert_error(
        tree::export_quiesced(&store, &snapshot, &inside_cas, &ImportBudget::default())
            .unwrap_err(),
        "inside the live CAS",
    );
    assert!(!inside_cas.exists());
    let limited = temp.path().join("limited");
    assert_error(
        tree::export_quiesced(
            &store,
            &snapshot,
            &limited,
            &ImportBudget {
                max_logical_bytes: 0,
                ..ImportBudget::default()
            },
        )
        .unwrap_err(),
        "logical bytes",
    );
    assert!(!limited.exists());
}

#[test]
fn export_applies_mode_zero_only_after_cross_directory_hardlinks_are_complete() {
    let temp = scratch();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let blob = store.put_blob(b"closed permissions").unwrap();
    let a = paged(&store, &stage.id, 0, vec![file(b"file", &blob, 0)]);
    let z = legacy(
        &store,
        0o711,
        vec![store::DirectoryEntry::new(
            b"file",
            store::EntryNode::File {
                object: blob,
                mode: 0,
            },
        )
        .unwrap()],
    );
    let root = paged(
        &store,
        &stage.id,
        0o750,
        vec![
            DirectoryEntry::new(b"a", EntryNode::Directory { object: a }).unwrap(),
            DirectoryEntry::new(b"z", EntryNode::Directory { object: z }).unwrap(),
        ],
    );
    let snapshot = snapshot(
        &store,
        root,
        vec![vec![vec![b"a", b"file"], vec![b"z", b"file"]]],
    );
    let target = temp.path().join("export");
    tree::export_quiesced(&store, &snapshot, &target, &ImportBudget::default()).unwrap();
    assert_eq!((mode(&target), mode(&target.join("a"))), (0o750, 0));
    fs::set_permissions(target.join("a"), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(mode(&target.join("a/file")), 0);
    assert_eq!(
        fs::metadata(target.join("a/file")).unwrap().ino(),
        fs::metadata(target.join("z/file")).unwrap().ino()
    );
    fs::set_permissions(target.join("a/file"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fs::read(target.join("z/file")).unwrap(),
        b"closed permissions"
    );
}

#[test]
fn physical_raw_names_and_symlink_targets_preserve_bytes_on_supported_filesystems() {
    let temp = scratch();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let raw = OsString::from_vec(vec![0xff, b'r', b'a', b'w']);
    let physical_name = match fs::write(source.join(&raw), b"raw name") {
        Ok(()) => raw,
        Err(error)
            if cfg!(target_os = "macos")
                && matches!(
                    error.raw_os_error(),
                    Some(libc::EINVAL) | Some(libc::EILSEQ)
                ) =>
        {
            let name = OsString::from("unicode-λ");
            fs::write(source.join(&name), b"raw name").unwrap();
            // APFS may normalize Unicode; use its actual raw directory entry.
            fs::read_dir(&source)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name()
        }
        Err(error) => panic!("physical name creation failed: {error}"),
    };
    let link_target = OsString::from_vec(vec![b'.', b'.', b'/', 0xff]);
    symlink(&link_target, source.join("link")).unwrap();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let snapshot = tree::import_quiesced(
        &store,
        &stage.id,
        &source,
        &ImportPolicy::default(),
        &ImportBudget::default(),
    )
    .unwrap();
    assert!(
        tree::lookup(&store, &snapshot.directory, physical_name.as_bytes())
            .unwrap()
            .is_some()
    );
    let target = temp.path().join("export");
    tree::export_quiesced(&store, &snapshot, &target, &ImportBudget::default()).unwrap();
    assert_eq!(fs::read(target.join(physical_name)).unwrap(), b"raw name");
    assert_eq!(
        fs::read_link(target.join("link"))
            .unwrap()
            .as_os_str()
            .as_bytes(),
        link_target.as_bytes()
    );
}
