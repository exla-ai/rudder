use rudder_engine::{
    protocol::{canonical_json, ObjectRef, OwnerEpoch},
    store::{DirectoryEntry, DirectoryManifest, EntryNode, LocalStore},
    vfs::{errno, HardlinkGroup, HardlinkTopology, NodeKind, VfsCheckpoint, VfsManager, VfsView},
};
use std::{
    fs,
    time::{Duration, Instant},
};
use uuid::Uuid;

fn owner() -> OwnerEpoch {
    OwnerEpoch {
        owner: "vfs-test".into(),
        epoch: 1,
    }
}
fn directory(s: &LocalStore, entries: Vec<DirectoryEntry>) -> ObjectRef {
    s.put_directory(DirectoryManifest {
        format_version: 1,
        mode: 0o755,
        entries,
    })
    .unwrap()
}
fn file(s: &LocalStore, name: &[u8], bytes: &[u8]) -> DirectoryEntry {
    DirectoryEntry::new(
        name,
        EntryNode::File {
            object: s.put_blob(bytes).unwrap(),
            mode: 0o644,
        },
    )
    .unwrap()
}
fn setup() -> (tempfile::TempDir, LocalStore, VfsManager) {
    let root = std::env::var_os("RUDDER_TEST_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            if std::path::Path::new("/workspace").is_dir() {
                "/workspace".into()
            } else {
                std::env::temp_dir()
            }
        });
    let t = tempfile::Builder::new()
        .prefix("rudder-vfs-")
        .tempdir_in(root)
        .unwrap();
    let s = LocalStore::open(t.path().join("cas")).unwrap();
    let m = VfsManager::open(t.path().join("manager"), s.clone()).unwrap();
    (t, s, m)
}
fn fork(m: &VfsManager, r: &ObjectRef) -> VfsView {
    m.prepare(r).unwrap();
    m.fork(&Uuid::new_v4().to_string(), r, owner()).unwrap()
}

#[test]
fn immutable_forks_keep_equal_files_and_directories_distinct() {
    let (_t, s, m) = setup();
    let empty = directory(&s, vec![]);
    let root = directory(
        &s,
        vec![
            file(&s, b"a", b"same"),
            file(&s, b"b", b"same"),
            DirectoryEntry::new(
                b"d1",
                EntryNode::Directory {
                    object: empty.clone(),
                },
            )
            .unwrap(),
            DirectoryEntry::new(b"d2", EntryNode::Directory { object: empty }).unwrap(),
        ],
    );
    let a = fork(&m, &root);
    let b = fork(&m, &root);
    assert_eq!(a.descriptor().unwrap().inode_count, 1);
    assert_eq!(a.descriptor().unwrap().private_dirent_count, 0);
    let one = a.lookup(1, b"a").unwrap();
    let two = a.lookup(1, b"b").unwrap();
    assert_ne!(one.ino, two.ino);
    assert_ne!(
        a.lookup(1, b"d1").unwrap().ino,
        a.lookup(1, b"d2").unwrap().ino
    );
    a.write(one.ino, 0, b"mine", true).unwrap();
    assert_eq!(a.read(one.ino, 0, 10).unwrap(), b"mine");
    assert_eq!(a.read(two.ino, 0, 10).unwrap(), b"same");
    let other = b.lookup(1, b"a").unwrap();
    assert_eq!(b.read(other.ino, 0, 10).unwrap(), b"same");
    assert_eq!(
        s.read_object(match &s.read_directory(&root).unwrap().entries[0].node {
            EntryNode::File { object, .. } => object,
            _ => panic!(),
        })
        .unwrap(),
        b"same"
    );
}

#[test]
fn inherited_directory_rename_does_not_visit_descendants() {
    let (_t, s, m) = setup();
    let blob = s.put_blob(b"payload").unwrap();
    let inherited = directory(
        &s,
        (0..10_000)
            .map(|i| {
                DirectoryEntry::new(
                    format!("f{i:05}").as_bytes(),
                    EntryNode::File {
                        object: blob.clone(),
                        mode: 0o644,
                    },
                )
                .unwrap()
            })
            .collect(),
    );
    let dst = directory(&s, vec![]);
    let root = directory(
        &s,
        vec![
            DirectoryEntry::new(b"inherited", EntryNode::Directory { object: inherited }).unwrap(),
            DirectoryEntry::new(b"destination", EntryNode::Directory { object: dst }).unwrap(),
        ],
    );
    let v = fork(&m, &root);
    let p = v.lookup(1, b"destination").unwrap();
    v.rename(1, b"inherited", p.ino, b"moved", 0).unwrap();
    assert_eq!(v.descriptor().unwrap().inode_count, 3);
    assert_eq!(
        v.lookup(1, b"inherited")
            .unwrap_err()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(libc::ENOENT)
    );
    let d = v.lookup(p.ino, b"moved").unwrap();
    assert_eq!(
        v.read(v.lookup(d.ino, b"f09999").unwrap().ino, 0, 20)
            .unwrap(),
        b"payload"
    );
    assert!(v.rename(p.ino, b"moved", d.ino, b"cycle", 0).is_err());
}

#[test]
fn handles_survive_unlink_and_replacement_and_runtime_hardlinks_share_inode() {
    let (_t, s, m) = setup();
    let root = directory(
        &s,
        vec![file(&s, b"a", b"one"), file(&s, b"replace", b"two")],
    );
    let v = fork(&m, &root);
    let a = v.lookup(1, b"a").unwrap();
    let fd = v.open_handle(a.ino, libc::O_RDWR, false).unwrap();
    v.link(a.ino, 1, b"alias").unwrap();
    assert_eq!(v.lookup(1, b"alias").unwrap().ino, a.ino);
    v.write(a.ino, 0, b"NEW", true).unwrap();
    assert_eq!(
        v.read(v.lookup(1, b"alias").unwrap().ino, 0, 10).unwrap(),
        b"NEW"
    );
    v.unlink(1, b"a", false).unwrap();
    v.rename(1, b"replace", 1, b"alias", 0).unwrap();
    assert_eq!(v.attr(a.ino).unwrap().nlink, 0);
    v.check_handle(a.ino, fd, true).unwrap();
    assert_eq!(v.read(a.ino, 0, 10).unwrap(), b"NEW");
    assert!(v.checkpoint_snapshot_unmounted().is_err());
    v.close_handle(a.ino, fd).unwrap();
    let cp = v.checkpoint_snapshot_unmounted().unwrap();
    let entries = s.read_directory(&cp.directory).unwrap().entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name().unwrap(), b"alias");
}

#[test]
fn raw_names_symlinks_modes_and_checkpoint_retention() {
    let (_t, s, m) = setup();
    let root = directory(&s, vec![]);
    let v = fork(&m, &root);
    let f = v
        .create(1, b"raw-\xff", NodeKind::File, 0o751, b"")
        .unwrap();
    v.write(f.ino, 0, b"body", true).unwrap();
    v.create(1, b"link", NodeKind::Symlink, 0o777, b"raw-\xff")
        .unwrap();
    assert_eq!(
        v.readlink(v.lookup(1, b"link").unwrap().ino).unwrap(),
        b"raw-\xff"
    );
    for name in [&b".."[..], &b"a/b"[..], &b"a\0b"[..]] {
        assert!(v.create(1, name, NodeKind::File, 0o600, b"").is_err())
    }
    assert!(v
        .link(v.lookup(1, b"link").unwrap().ino, 1, b"sym-hard")
        .is_err());
    let cp = v.checkpoint_snapshot_unmounted().unwrap();
    s.gc(Duration::ZERO).unwrap();
    assert!(s.read_directory(&cp.directory).is_ok());
    assert!(s.read_object(&cp.hardlinks).is_ok());
    m.prepare_snapshot(&cp).unwrap();
    let restored = m
        .fork_snapshot(&Uuid::new_v4().to_string(), &cp, owner())
        .unwrap();
    let attr = restored.lookup(1, b"raw-\xff").unwrap();
    assert_eq!(attr.mode, 0o751);
    assert_eq!(restored.read(attr.ino, 0, 20).unwrap(), b"body");
}

#[test]
fn topology_roundtrip_survives_ancestor_rename_and_unlooked_alias_mutation() {
    let (_t, s, m) = setup();
    let root = directory(&s, vec![file(&s, b"source", b"first")]);
    let v = fork(&m, &root);
    let a = v.lookup(1, b"source").unwrap();
    let d = v
        .create(1, b"dir", NodeKind::Directory, 0o755, b"")
        .unwrap();
    v.link(a.ino, d.ino, b"alias").unwrap();
    let cp = v.checkpoint_snapshot_unmounted().unwrap();
    m.prepare_snapshot(&cp).unwrap();
    let r = m
        .fork_snapshot(&Uuid::new_v4().to_string(), &cp, owner())
        .unwrap();
    assert_eq!(r.descriptor().unwrap().inode_count, 1);
    r.rename(1, b"dir", 1, b"renamed", 0).unwrap();
    assert_eq!(r.descriptor().unwrap().inode_count, 2);
    let source = r.lookup(1, b"source").unwrap();
    r.write(source.ino, 0, b"after", true).unwrap();
    let moved = r.lookup(1, b"renamed").unwrap();
    let alias = r.lookup(moved.ino, b"alias").unwrap();
    assert_eq!(source.ino, alias.ino);
    assert_eq!(r.read(alias.ino, 0, 10).unwrap(), b"after");
    let next = r.checkpoint_snapshot_unmounted().unwrap();
    m.prepare_snapshot(&next).unwrap();
    let rr = m
        .fork_snapshot(&Uuid::new_v4().to_string(), &next, owner())
        .unwrap();
    let src = rr.lookup(1, b"source").unwrap();
    let d = rr.lookup(1, b"renamed").unwrap();
    assert_eq!(src.ino, rr.lookup(d.ino, b"alias").unwrap().ino);
}

#[test]
fn topology_rejects_unsafe_inconsistent_and_duplicate_groups() {
    let (_t, s, m) = setup();
    let root = directory(&s, vec![file(&s, b"a", b"a"), file(&s, b"b", b"different")]);
    m.prepare(&root).unwrap();
    let top = HardlinkTopology {
        format_version: 1,
        groups: vec![HardlinkGroup {
            paths_hex: vec![vec![hex::encode(b"a")], vec![hex::encode(b"b")]],
        }],
    };
    let cp = VfsCheckpoint {
        format_version: 1,
        directory: root.clone(),
        hardlinks: s.put_blob(&canonical_json(&top).unwrap()).unwrap(),
    };
    assert!(m.prepare_snapshot(&cp).is_err());
    let top = HardlinkTopology {
        format_version: 1,
        groups: vec![HardlinkGroup {
            paths_hex: vec![vec!["2e2e".into()], vec![hex::encode(b"a")]],
        }],
    };
    let cp = VfsCheckpoint {
        format_version: 1,
        directory: root,
        hardlinks: s.put_blob(&canonical_json(&top).unwrap()).unwrap(),
    };
    assert!(m.prepare_snapshot(&cp).is_err());
}

#[test]
fn exchange_and_noreplace_preserve_namespace_on_failure() {
    let (_t, s, m) = setup();
    let root = directory(&s, vec![file(&s, b"a", b"A"), file(&s, b"b", b"B")]);
    let v = fork(&m, &root);
    let a = v.lookup(1, b"a").unwrap();
    let b = v.lookup(1, b"b").unwrap();
    assert_eq!(
        errno(&v.rename(1, b"a", 1, b"b", 1).unwrap_err()),
        libc::EEXIST
    );
    assert_eq!(v.lookup(1, b"a").unwrap().ino, a.ino);
    v.rename(1, b"a", 1, b"b", 2).unwrap();
    assert_eq!(v.lookup(1, b"a").unwrap().ino, b.ino);
    assert_eq!(v.lookup(1, b"b").unwrap().ino, a.ino);
    assert_eq!(
        errno(&v.rename(1, b"a", 1, b"missing", 2).unwrap_err()),
        libc::ENOENT
    );
}

#[test]
fn missing_private_only_copy_and_wrong_owner_fail_closed() {
    let (t, s, m) = setup();
    let root = directory(&s, vec![file(&s, b"a", b"A")]);
    let id = Uuid::new_v4().to_string();
    m.prepare(&root).unwrap();
    let v = m.fork(&id, &root, owner()).unwrap();
    let a = v.lookup(1, b"a").unwrap();
    v.write(a.ino, 0, b"B", true).unwrap();
    assert!(m.open_view(&id, &owner()).is_err());
    drop(v);
    assert!(m
        .open_view(
            &id,
            &OwnerEpoch {
                owner: "other".into(),
                epoch: 2
            }
        )
        .is_err());
    let files = t.path().join("manager/views").join(&id).join("files");
    for e in fs::read_dir(files).unwrap() {
        fs::remove_file(e.unwrap().path()).unwrap();
    }
    assert!(m.open_view(&id, &owner()).is_err());
}

#[test]
fn vfs_crash_child() {
    let Ok(path) = std::env::var("RUDDER_VFS_CRASH_ROOT") else {
        return;
    };
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &limit);
    }
    let id = std::env::var("RUDDER_VFS_CRASH_ID").unwrap();
    let s = LocalStore::open(std::path::Path::new(&path).join("cas")).unwrap();
    let m = VfsManager::open(std::path::Path::new(&path).join("manager"), s).unwrap();
    let v = m.open_view(&id, &owner()).unwrap();
    let a = v.lookup(1, b"a").unwrap();
    v.write(a.ino, 0, b"durable", true).unwrap();
    v.rename(1, b"a", 1, b"acknowledged", 0).unwrap();
    v.sync(a.ino).unwrap();
    std::process::abort();
}
#[test]
fn acknowledged_namespace_and_fsync_survive_process_death() {
    let (t, s, m) = setup();
    let root = directory(&s, vec![file(&s, b"a", b"original")]);
    let id = Uuid::new_v4().to_string();
    m.prepare(&root).unwrap();
    drop(m.fork(&id, &root, owner()).unwrap());
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "vfs_crash_child", "--nocapture"])
        .env("RUDDER_VFS_CRASH_ROOT", t.path())
        .env("RUDDER_VFS_CRASH_ID", &id)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let v = m.open_view(&id, &owner()).unwrap();
    assert_eq!(errno(&v.lookup(1, b"a").unwrap_err()), libc::ENOENT);
    let a = v.lookup(1, b"acknowledged").unwrap();
    assert_eq!(v.read(a.ino, 0, 20).unwrap(), b"durablel");
}

#[test]
fn import_preserves_real_hardlinks_without_inventing_links_for_equal_files() {
    let (t, _s, m) = setup();
    let input = t.path().join("input");
    fs::create_dir(&input).unwrap();
    fs::write(input.join("a"), b"same").unwrap();
    fs::hard_link(input.join("a"), input.join("alias")).unwrap();
    fs::write(input.join("independent"), b"same").unwrap();
    let cp = m
        .prepare_from_quiesced_path(&input, &Default::default())
        .unwrap();
    let v = m
        .fork_snapshot(&Uuid::new_v4().to_string(), &cp, owner())
        .unwrap();
    let a = v.lookup(1, b"a").unwrap();
    assert_eq!(a.ino, v.lookup(1, b"alias").unwrap().ino);
    assert_ne!(a.ino, v.lookup(1, b"independent").unwrap().ino);
    v.write(a.ino, 0, b"edit", true).unwrap();
    assert_eq!(fs::read(input.join("a")).unwrap(), b"same");
    assert_eq!(
        v.read(v.lookup(1, b"alias").unwrap().ino, 0, 10).unwrap(),
        b"edit"
    );
}

#[test]
fn seal_and_destroy_are_idempotent_and_keep_recovery_roots() {
    let (_t, s, m) = setup();
    let root = directory(&s, vec![file(&s, b"a", b"first")]);
    let v = fork(&m, &root);
    let a = v.lookup(1, b"a").unwrap();
    v.write(a.ino, 0, b"other", true).unwrap();
    let cp = v.seal().unwrap();
    assert_eq!(v.seal().unwrap(), cp);
    assert_eq!(
        errno(&v.write(a.ino, 0, b"bad", false).unwrap_err()),
        libc::EROFS
    );
    assert!(v.rename(1, b"a", 1, b"b", 0).is_err());
    assert_eq!(v.destroy().unwrap(), cp);
    assert_eq!(v.destroy().unwrap(), cp);
    s.gc(Duration::ZERO).unwrap();
    assert_eq!(v.descriptor().unwrap().latest_snapshot, Some(cp.clone()));
    m.prepare_snapshot(&cp).unwrap();
    assert!(s.read_object(&cp.hardlinks).is_ok());
}

#[test]
fn rename_database_failure_rolls_back_replacement_and_link_counts() {
    let (_t, s, m) = setup();
    let root = directory(&s, vec![file(&s, b"a", b"A"), file(&s, b"b", b"B")]);
    let v = fork(&m, &root);
    let a = v.lookup(1, b"a").unwrap();
    let b = v.lookup(1, b"b").unwrap();
    let db =
        rusqlite::Connection::open(m.root().join("views").join(v.id()).join("namespace.sqlite"))
            .unwrap();
    db.execute_batch("CREATE TRIGGER fail_replacement BEFORE INSERT ON dirents WHEN NEW.name=x'62' BEGIN SELECT RAISE(FAIL,'injected storage failure'); END;").unwrap();
    assert!(v.rename(1, b"a", 1, b"b", 0).is_err());
    assert_eq!(v.lookup(1, b"a").unwrap().ino, a.ino);
    assert_eq!(v.lookup(1, b"b").unwrap().ino, b.ino);
    assert_eq!(v.attr(b.ino).unwrap().nlink, 1);
    db.execute_batch("DROP TRIGGER fail_replacement;").unwrap();
    v.rename(1, b"a", 1, b"b", 0).unwrap();
    assert_eq!(v.attr(b.ino).unwrap().nlink, 0);
}

#[test]
fn explicit_timestamps_survive_chmod_and_are_not_replaced_by_dirty_bookkeeping() {
    let (_t, s, m) = setup();
    let root = directory(&s, vec![file(&s, b"a", b"A")]);
    let v = fork(&m, &root);
    let a = v.lookup(1, b"a").unwrap();
    let changed = v.set_attr(a.ino, None, None, Some(12), Some(34)).unwrap();
    assert_eq!(changed.atime_ns, 12);
    assert_eq!(changed.mtime_ns, 34);
    let changed = v.set_attr(a.ino, Some(0o755), None, None, None).unwrap();
    assert_eq!(changed.mtime_ns, 34);
}

#[test]
fn directory_link_counts_follow_create_cross_parent_rename_and_removal() {
    let (_t, s, m) = setup();
    let base = directory(&s, vec![]);
    let v = fork(&m, &base);
    assert_eq!(v.attr(1).unwrap().nlink, 2);
    let a = v.create(1, b"a", NodeKind::Directory, 0o755, b"").unwrap();
    assert_eq!(a.nlink, 2);
    assert_eq!(v.attr(1).unwrap().nlink, 3);
    let b = v
        .create(a.ino, b"b", NodeKind::Directory, 0o755, b"")
        .unwrap();
    assert_eq!(v.attr(a.ino).unwrap().nlink, 3);
    v.rename(a.ino, b"b", 1, b"moved", 0).unwrap();
    assert_eq!(v.attr(a.ino).unwrap().nlink, 2);
    assert_eq!(v.attr(1).unwrap().nlink, 4);
    v.unlink(1, b"moved", true).unwrap();
    assert_eq!(v.attr(b.ino).unwrap().nlink, 0);
    assert_eq!(v.attr(1).unwrap().nlink, 3);
    v.unlink(1, b"a", true).unwrap();
    assert_eq!(v.attr(1).unwrap().nlink, 2);
}

#[test]
#[ignore = "scaling evidence; run explicitly on disposable workspace"]
fn fork_scaling_evidence() {
    let (_t, s, m) = setup();
    let blob = s.put_blob(b"payload").unwrap();
    let mut evidence = Vec::new();
    for count in [1, 100, 10_000, 100_000] {
        let inherited = directory(
            &s,
            (0..count)
                .map(|i| {
                    DirectoryEntry::new(
                        format!("f{i:06}").as_bytes(),
                        EntryNode::File {
                            object: blob.clone(),
                            mode: 0o644,
                        },
                    )
                    .unwrap()
                })
                .collect(),
        );
        let root = directory(
            &s,
            vec![DirectoryEntry::new(b"tree", EntryNode::Directory { object: inherited }).unwrap()],
        );
        let start = Instant::now();
        m.prepare(&root).unwrap();
        let prepared = start.elapsed();
        let start = Instant::now();
        let v = m.fork(&Uuid::new_v4().to_string(), &root, owner()).unwrap();
        let fork = start.elapsed();
        let start = Instant::now();
        v.rename(1, b"tree", 1, b"renamed", 0).unwrap();
        let rename = start.elapsed();
        let d = v.descriptor().unwrap();
        assert_eq!(d.inode_count, 2);
        evidence.push(serde_json::json!({"entries":count,"prepare_ns":prepared.as_nanos(),"fork_ns":fork.as_nanos(),"rename_ns":rename.as_nanos(),"inodes_after_rename":d.inode_count}));
    }
    println!("{}", serde_json::json!({"logical_vfs_scaling":evidence}));
}
