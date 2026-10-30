use rudder_engine::{
    content::{self, ContentStore, FileNode, StagedContentStore, CHUNK_BYTES, FILE_LAYOUT},
    directory::{DirectoryEntry, EntryNode, LocalDirectoryStore, PagedDirectory},
    protocol::{ObjectKind, ObjectRef, OwnerEpoch},
    store::LocalStore,
    vfs::{NodeKind, VfsManager, VfsView},
};
use std::{fs, time::Duration};
use uuid::Uuid;

fn owner() -> OwnerEpoch {
    OwnerEpoch {
        owner: "ranges-test".into(),
        epoch: 7,
    }
}
fn setup() -> (tempfile::TempDir, LocalStore, VfsManager) {
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
        .prefix("rudder-vfs-ranges-")
        .tempdir_in(base)
        .unwrap();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let manager = VfsManager::open(temp.path().join("manager"), store.clone()).unwrap();
    (temp, store, manager)
}
fn paged(store: &LocalStore, entries: Vec<DirectoryEntry>) -> ObjectRef {
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let adapter = LocalDirectoryStore::new(store, &stage.id);
    let root = PagedDirectory::new(&adapter).build(0o755, entries).unwrap();
    store
        .publish(&stage.id, &format!("fixture:{}", root.digest), &root)
        .unwrap();
    root
}
fn fork(manager: &VfsManager, root: &ObjectRef) -> VfsView {
    manager.prepare_paged(root).unwrap();
    manager
        .fork_paged(&Uuid::new_v4().to_string(), root, owner())
        .unwrap()
}
fn log_bytes(manager: &VfsManager, view: &VfsView) -> u64 {
    fs::read_dir(manager.root().join("views").join(view.id()).join("files"))
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum()
}
fn repeated_file(store: &LocalStore, count: usize) -> (ObjectRef, ObjectRef) {
    let stage = store.begin_staging(Duration::from_secs(600)).unwrap();
    let adapter = StagedContentStore::new(store, &stage.id).unwrap();
    let chunk = adapter.put_blob(&vec![b'q'; CHUNK_BYTES]).unwrap();
    let file = adapter
        .put_file_node(&FileNode {
            format_version: 1,
            layout: FILE_LAYOUT.into(),
            level: 0,
            logical_size: CHUNK_BYTES as u64 * count as u64,
            children: vec![chunk.clone(); count],
        })
        .unwrap();
    store
        .publish(&stage.id, &format!("fixture-file:{}", file.digest), &file)
        .unwrap();
    (file, chunk)
}

#[test]
fn inherited_large_file_range_copyup_and_checkpoint_reuse_unchanged_chunks() {
    let (_temp, store, manager) = setup();
    let (file, chunk) = repeated_file(&store, 52); //208MiB logical,4MiB raw payload
    let root = paged(
        &store,
        vec![DirectoryEntry::new(
            b"large",
            EntryNode::File {
                object: file.clone(),
                logical_size: 52 * CHUNK_BYTES as u64,
                mode: 0o751,
            },
        )
        .unwrap()],
    );
    let view = fork(&manager, &root);
    let sibling = fork(&manager, &root);
    assert_eq!(view.descriptor().unwrap().inode_count, 1);
    let inode = view.lookup(1, b"large").unwrap();
    assert_eq!(inode.size, 52 * CHUNK_BYTES as u64);
    assert_ne!(inode.size, file.size_bytes);
    assert_eq!(log_bytes(&manager, &view), 0);
    assert_eq!(
        view.read(inode.ino, CHUNK_BYTES as u64 - 3, 6).unwrap(),
        b"qqqqqq"
    );
    view.write(inode.ino, 11, b"changed", false).unwrap();
    assert_eq!(log_bytes(&manager, &view), 64 * 1024);
    assert_eq!(view.attr(inode.ino).unwrap().size, inode.size);
    assert_eq!(view.read(inode.ino, 9, 11).unwrap(), b"qqchangedqq");
    assert_eq!(
        sibling
            .read(sibling.lookup(1, b"large").unwrap().ino, 9, 11)
            .unwrap(),
        b"qqqqqqqqqqq"
    );
    let snapshot = view.checkpoint_snapshot_unmounted().unwrap();
    let entry = rudder_engine::tree::lookup(&store, &snapshot.directory, b"large")
        .unwrap()
        .unwrap();
    let EntryNode::File {
        object,
        logical_size,
        mode,
    } = entry.node
    else {
        panic!()
    };
    assert_eq!(logical_size, inode.size);
    assert_eq!(mode, 0o751);
    assert_eq!(object.kind, ObjectKind::File);
    let descriptor = content::decode_file_node(&store.read_object(&object).unwrap()).unwrap();
    assert_eq!(descriptor.children.len(), 52);
    assert_ne!(descriptor.children[0], chunk);
    assert!(descriptor.children[1..].iter().all(|child| child == &chunk));
    assert_eq!(view.descriptor().unwrap().inode_count, 2);
    assert_eq!(log_bytes(&manager, &view), 64 * 1024);
    assert_eq!(view.checkpoint_snapshot_unmounted().unwrap(), snapshot);
    view.write(inode.ino, CHUNK_BYTES as u64 + 5, b"next", true)
        .unwrap();
    assert_eq!(view.read(inode.ino, 11, 7).unwrap(), b"changed");
    assert_eq!(
        view.read(inode.ino, CHUNK_BYTES as u64 + 5, 4).unwrap(),
        b"next"
    );
}

#[test]
fn truncate_extend_holes_and_sparse_terabyte_do_not_resurrect_inherited_bytes() {
    let (_temp, store, manager) = setup();
    let blob = store.put_blob(b"inherited-secret").unwrap();
    let root = paged(
        &store,
        vec![DirectoryEntry::new(
            b"data",
            EntryNode::File {
                logical_size: blob.size_bytes,
                object: blob,
                mode: 0o600,
            },
        )
        .unwrap()],
    );
    let view = fork(&manager, &root);
    let file = view.lookup(1, b"data").unwrap();
    view.write(file.ino, 2, b"XX", true).unwrap();
    view.set_attr(file.ino, None, Some(3), None, None).unwrap();
    view.set_attr(file.ino, None, Some(20), None, None).unwrap();
    let mut expected = b"inX".to_vec();
    expected.resize(20, 0);
    assert_eq!(view.read(file.ino, 0, 30).unwrap(), expected);
    view.write(file.ino, 19, b"Z", false).unwrap();
    expected[19] = b'Z';
    assert_eq!(view.read(file.ino, 0, 30).unwrap(), expected);
    let snapshot = view.checkpoint_snapshot_unmounted().unwrap();
    manager.prepare_paged_snapshot(&snapshot).unwrap();
    let restored = manager
        .fork_paged_snapshot(&Uuid::new_v4().to_string(), &snapshot, owner())
        .unwrap();
    assert_eq!(
        restored
            .read(restored.lookup(1, b"data").unwrap().ino, 0, 30)
            .unwrap(),
        expected
    );
    let sparse = view
        .create(1, b"sparse", NodeKind::File, 0o600, b"")
        .unwrap();
    view.set_attr(sparse.ino, None, Some(1u64 << 40), None, None)
        .unwrap();
    assert_eq!(view.read(sparse.ino, (1u64 << 40) - 4, 10).unwrap(), [0; 4]);
    view.write(sparse.ino, (1u64 << 40) - 2, b"OK", false)
        .unwrap();
    assert_eq!(log_bytes(&manager, &view), 4 * 64 * 1024);
    let sealed = view.checkpoint_snapshot_unmounted().unwrap();
    let entry = rudder_engine::tree::lookup(&store, &sealed.directory, b"sparse")
        .unwrap()
        .unwrap();
    let EntryNode::File {
        object,
        logical_size,
        ..
    } = entry.node
    else {
        panic!()
    };
    assert_eq!(logical_size, 1u64 << 40);
    assert_eq!(
        content::read_file_range(&store, &object, logical_size - 4, 4).unwrap(),
        b"\0\0OK"
    );
    assert_eq!(
        content::read_file_range(&store, &object, CHUNK_BYTES as u64 * 1031, 4).unwrap(),
        [0; 4]
    );
}

#[test]
fn durable_private_log_reopens_and_corruption_never_falls_back_to_base() {
    let (_temp, store, manager) = setup();
    let blob = store.put_blob(b"immutable").unwrap();
    let root = paged(
        &store,
        vec![DirectoryEntry::new(
            b"file",
            EntryNode::File {
                logical_size: blob.size_bytes,
                object: blob,
                mode: 0o644,
            },
        )
        .unwrap()],
    );
    let view = fork(&manager, &root);
    let id = view.id().to_owned();
    let inode = view.lookup(1, b"file").unwrap();
    view.write(inode.ino, 0, b"changed", false).unwrap();
    view.sync(inode.ino).unwrap();
    drop(view);
    let view = manager.open_view(&id, &owner()).unwrap();
    assert_eq!(view.read(inode.ino, 0, 20).unwrap(), b"changedle");
    let path = fs::read_dir(manager.root().join("views").join(&id).join("files"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    use std::os::unix::fs::FileExt;
    let log = fs::OpenOptions::new().write(true).open(path).unwrap();
    log.write_all_at(b"tamper", 0).unwrap();
    log.sync_all().unwrap();
    assert!(view
        .read(inode.ino, 0, 20)
        .unwrap_err()
        .to_string()
        .contains("checksum"));
    assert!(view.checkpoint_snapshot_unmounted().is_err());
}

#[test]
fn paged_hardlinks_open_unlink_and_ancestor_move_roundtrip() {
    let (_temp, store, manager) = setup();
    let root = paged(&store, vec![]);
    let view = fork(&manager, &root);
    let file = view
        .create(1, b"source", NodeKind::File, 0o640, b"")
        .unwrap();
    view.write(file.ino, 0, b"first", true).unwrap();
    let dir = view
        .create(1, b"dir", NodeKind::Directory, 0o755, b"")
        .unwrap();
    view.link(file.ino, dir.ino, b"alias").unwrap();
    let snapshot = view.checkpoint_snapshot_unmounted().unwrap();
    manager.prepare_paged_snapshot(&snapshot).unwrap();
    let restored = manager
        .fork_paged_snapshot(&Uuid::new_v4().to_string(), &snapshot, owner())
        .unwrap();
    restored.rename(1, b"dir", 1, b"moved", 0).unwrap();
    let file = restored.lookup(1, b"source").unwrap();
    restored.write(file.ino, 0, b"after", false).unwrap();
    let dir = restored.lookup(1, b"moved").unwrap();
    assert_eq!(restored.lookup(dir.ino, b"alias").unwrap().ino, file.ino);
    let fd = restored.open_handle(file.ino, libc::O_RDWR, false).unwrap();
    restored.unlink(1, b"source", false).unwrap();
    restored.unlink(dir.ino, b"alias", false).unwrap();
    assert_eq!(restored.attr(file.ino).unwrap().nlink, 0);
    assert_eq!(restored.read(file.ino, 0, 10).unwrap(), b"after");
    assert!(restored.checkpoint_snapshot_unmounted().is_err());
    restored.close_handle(file.ino, fd).unwrap();
    let snapshot = restored.checkpoint_snapshot_unmounted().unwrap();
    assert!(rudder_engine::vfs::validate_snapshot(&store, &snapshot)
        .unwrap()
        .groups
        .is_empty());
}

#[test]
fn large_flat_directory_checkpoint_visits_only_changed_namespace() {
    let (_temp, store, manager) = setup();
    let blob = store.put_blob(b"x").unwrap();
    let root = paged(
        &store,
        (0..25_000)
            .map(|index| {
                DirectoryEntry::new(
                    format!("file-{index:05}").as_bytes(),
                    EntryNode::File {
                        object: blob.clone(),
                        logical_size: 1,
                        mode: 0o644,
                    },
                )
                .unwrap()
            })
            .collect(),
    );
    let view = fork(&manager, &root);
    assert_eq!(view.descriptor().unwrap().inode_count, 1);
    view.rename(1, b"file-24999", 1, b"early", 0).unwrap();
    let file = view.lookup(1, b"file-10000").unwrap();
    view.write(file.ino, 0, b"Y", true).unwrap();
    assert_eq!(view.descriptor().unwrap().inode_count, 3);
    let snapshot = view.checkpoint_snapshot_unmounted().unwrap();
    assert_eq!(view.descriptor().unwrap().inode_count, 3);
    assert!(
        rudder_engine::tree::lookup(&store, &snapshot.directory, b"file-24999")
            .unwrap()
            .is_none()
    );
    assert!(
        rudder_engine::tree::lookup(&store, &snapshot.directory, b"early")
            .unwrap()
            .is_some()
    );
    let EntryNode::File { object, .. } =
        rudder_engine::tree::lookup(&store, &snapshot.directory, b"file-10000")
            .unwrap()
            .unwrap()
            .node
    else {
        panic!()
    };
    assert_eq!(store.read_object(&object).unwrap(), b"Y");
}

#[test]
fn failed_range_transaction_keeps_prior_pointer_and_skips_orphan_tail() {
    let (_temp, store, manager) = setup();
    let blob = store.put_blob(b"original").unwrap();
    let root = paged(
        &store,
        vec![DirectoryEntry::new(
            b"file",
            EntryNode::File {
                logical_size: blob.size_bytes,
                object: blob,
                mode: 0o644,
            },
        )
        .unwrap()],
    );
    let view = fork(&manager, &root);
    let inode = view.lookup(1, b"file").unwrap();
    view.write(inode.ino, 0, b"first", false).unwrap();
    let cursor = view.dirty_cursor().unwrap();
    let c = rusqlite::Connection::open(
        manager
            .root()
            .join("views")
            .join(view.id())
            .join("namespace.sqlite"),
    )
    .unwrap();
    c.execute_batch("CREATE TRIGGER fail_range BEFORE UPDATE OF size ON nodes BEGIN SELECT RAISE(ABORT,'injected range metadata failure'); END;").unwrap();
    assert!(view.write(inode.ino, 0, b"BAD", false).is_err());
    assert_eq!(view.dirty_cursor().unwrap(), cursor);
    assert_eq!(view.read(inode.ino, 0, 10).unwrap(), b"firstnal");
    assert_eq!(log_bytes(&manager, &view), 2 * 64 * 1024);
    c.execute_batch("DROP TRIGGER fail_range;").unwrap();
    view.write(inode.ino, 0, b"new", false).unwrap();
    assert_eq!(log_bytes(&manager, &view), 3 * 64 * 1024);
    assert_eq!(view.read(inode.ino, 0, 10).unwrap(), b"newstnal");
    let snapshot = view.checkpoint_snapshot_unmounted().unwrap();
    manager.prepare_paged_snapshot(&snapshot).unwrap();
    let restored = manager
        .fork_paged_snapshot(&Uuid::new_v4().to_string(), &snapshot, owner())
        .unwrap();
    assert_eq!(
        restored
            .read(restored.lookup(1, b"file").unwrap().ino, 0, 10)
            .unwrap(),
        b"newstnal"
    );
}

#[test]
fn checkpoint_receipt_failure_keeps_private_blocks_and_retry_can_reconcile_published_root() {
    let (_temp, store, manager) = setup();
    let root = paged(&store, vec![]);
    let view = fork(&manager, &root);
    let inode = view.create(1, b"file", NodeKind::File, 0o644, b"").unwrap();
    view.write(inode.ino, 0, b"only copy", false).unwrap();
    let c = rusqlite::Connection::open(
        manager
            .root()
            .join("views")
            .join(view.id())
            .join("namespace.sqlite"),
    )
    .unwrap();
    c.execute_batch("CREATE TRIGGER fail_rebase BEFORE UPDATE OF base ON nodes BEGIN SELECT RAISE(ABORT,'injected rebase receipt failure'); END;").unwrap();
    assert!(view.checkpoint_snapshot_unmounted().is_err());
    store.gc(Duration::ZERO).unwrap();
    assert_eq!(view.read(inode.ino, 0, 20).unwrap(), b"only copy");
    assert!(view.descriptor().unwrap().latest.is_none());
    c.execute_batch("DROP TRIGGER fail_rebase;").unwrap();
    let snapshot = view.checkpoint_snapshot_unmounted().unwrap();
    assert_eq!(view.checkpoint_snapshot_unmounted().unwrap(), snapshot);
    view.write(inode.ino, 0, b"next", false).unwrap();
    assert_eq!(view.read(inode.ino, 0, 20).unwrap(), b"next copy");
}

#[test]
fn ranges_crash_child() {
    let Ok(path) = std::env::var("RUDDER_VFS_RANGES_CRASH_ROOT") else {
        return;
    };
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &limit);
    }
    let store = LocalStore::open(std::path::Path::new(&path).join("cas")).unwrap();
    let manager = VfsManager::open(std::path::Path::new(&path).join("manager"), store).unwrap();
    let view = manager
        .open_view(
            &std::env::var("RUDDER_VFS_RANGES_CRASH_ID").unwrap(),
            &owner(),
        )
        .unwrap();
    let file = view.lookup(1, b"file").unwrap();
    view.write(file.ino, 3, b"ACK", false).unwrap();
    view.rename(1, b"file", 1, b"acknowledged", 0).unwrap();
    std::process::abort(); // no close/drop/sync call after acknowledged range write
}

#[test]
fn acknowledged_range_metadata_and_data_survive_unclean_process_exit() {
    let (temp, store, manager) = setup();
    let blob = store.put_blob(b"original").unwrap();
    let root = paged(
        &store,
        vec![DirectoryEntry::new(
            b"file",
            EntryNode::File {
                logical_size: blob.size_bytes,
                object: blob,
                mode: 0o644,
            },
        )
        .unwrap()],
    );
    let view = fork(&manager, &root);
    let id = view.id().to_string();
    drop(view);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "ranges_crash_child", "--nocapture"])
        .env("RUDDER_VFS_RANGES_CRASH_ROOT", temp.path())
        .env("RUDDER_VFS_RANGES_CRASH_ID", &id)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let recovered = manager.open_view(&id, &owner()).unwrap();
    assert!(recovered.lookup(1, b"file").is_err());
    let file = recovered.lookup(1, b"acknowledged").unwrap();
    assert_eq!(recovered.read(file.ino, 0, 10).unwrap(), b"oriACKal");
    recovered.checkpoint_snapshot_unmounted().unwrap();
}
