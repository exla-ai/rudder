use rudder_engine::{
    protocol::{ObjectRef, OwnerEpoch},
    store::{DirectoryEntry, DirectoryManifest, EntryNode, LocalStore},
    vfs::{
        linux::{mount, PrivateMountNamespace},
        VfsManager, VfsView,
    },
};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStringExt,
            fs::{symlink, MetadataExt, PermissionsExt},
        },
    },
    path::PathBuf,
    time::{Duration, Instant},
};
use uuid::Uuid;
fn emit_stage(phase: &str, entries: usize, start: Instant, details: serde_json::Value) {
    println!(
        "{}",
        serde_json::json!({"event":"vfs_stage","phase":phase,"entries":entries,"timestamp_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(),"elapsed_ns":start.elapsed().as_nanos(),"uid":unsafe{libc::geteuid()},"details":details})
    );
    std::io::stdout().flush().unwrap();
}
fn scaling_result(row: serde_json::Value) -> serde_json::Value {
    println!(
        "{}",
        serde_json::json!({"event":"vfs_scaling_result","uid":unsafe{libc::geteuid()},"result":row})
    );
    std::io::stdout().flush().unwrap();
    row
}
fn owner() -> OwnerEpoch {
    OwnerEpoch {
        owner: "qualified-fuse".into(),
        epoch: 1,
    }
}
fn ranges() -> bool {
    std::env::var("RUDDER_VFS_DATA_PLANE").is_ok_and(|value| value == "paged_ranges_v2")
}
fn prepare(m: &VfsManager, root: &ObjectRef) -> Result<(), String> {
    if ranges() {
        m.prepare_paged(root)
    } else {
        m.prepare(root)
    }
    .map_err(|error| error.to_string())
}
fn fork(m: &VfsManager, id: &str, root: &ObjectRef, owner: OwnerEpoch) -> Result<VfsView, String> {
    if ranges() {
        m.fork_paged(id, root, owner)
    } else {
        m.fork(id, root, owner)
    }
    .map_err(|error| error.to_string())
}
fn prepare_snapshot(
    m: &VfsManager,
    snapshot: &rudder_engine::vfs::VfsCheckpoint,
) -> Result<(), String> {
    if ranges() {
        m.prepare_paged_snapshot(snapshot)
    } else {
        m.prepare_snapshot(snapshot)
    }
    .map_err(|error| error.to_string())
}
fn fork_snapshot(
    m: &VfsManager,
    id: &str,
    snapshot: &rudder_engine::vfs::VfsCheckpoint,
    owner: OwnerEpoch,
) -> Result<VfsView, String> {
    if ranges() {
        m.fork_paged_snapshot(id, snapshot, owner)
    } else {
        m.fork_snapshot(id, snapshot, owner)
    }
    .map_err(|error| error.to_string())
}
fn paged(s: &LocalStore, entries: Vec<rudder_engine::directory::DirectoryEntry>) -> ObjectRef {
    let stage = s.begin_staging(Duration::from_secs(3600)).unwrap();
    let adapter = rudder_engine::directory::LocalDirectoryStore::new(s, &stage.id);
    let count = entries.len();
    let start = Instant::now();
    let root = rudder_engine::directory::PagedDirectory::new(&adapter)
        .build(0o755, entries)
        .unwrap();
    emit_stage(
        "fixture_paged_build",
        count,
        start,
        serde_json::json!({"root":root}),
    );
    let start = Instant::now();
    s.publish(&stage.id, &format!("fixture:{}", root.digest), &root)
        .unwrap();
    emit_stage(
        "fixture_initial_publication",
        count,
        start,
        serde_json::json!({"root":root}),
    );
    root
}
fn dir(s: &LocalStore, e: Vec<DirectoryEntry>) -> ObjectRef {
    if !ranges() {
        return s
            .put_directory(DirectoryManifest {
                format_version: 1,
                mode: 0o755,
                entries: e,
            })
            .unwrap();
    }
    let entries = e
        .into_iter()
        .map(|entry| {
            let name = entry.name().unwrap();
            let node = match entry.node {
                EntryNode::File { object, mode } => rudder_engine::directory::EntryNode::File {
                    logical_size: object.size_bytes,
                    object,
                    mode,
                },
                EntryNode::Directory { object } => {
                    rudder_engine::directory::EntryNode::Directory { object }
                }
                EntryNode::Symlink { target_hex } => {
                    rudder_engine::directory::EntryNode::Symlink { target_hex }
                }
            };
            rudder_engine::directory::DirectoryEntry::new(&name, node).unwrap()
        })
        .collect();
    paged(s, entries)
}
fn f(blob: &ObjectRef, name: &[u8]) -> DirectoryEntry {
    DirectoryEntry::new(
        name,
        EntryNode::File {
            object: blob.clone(),
            mode: 0o644,
        },
    )
    .unwrap()
}
pub fn main() {
    if std::env::args().nth(1).as_deref() == Some("--build-info") {
        println!("{{\"format_version\":1,\"qualification\":\"paged_ranges_v2\"}}");
        return;
    }
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &limit);
    };
    let a = std::env::args().collect::<Vec<_>>();
    let root = PathBuf::from(&a[2]);
    if a[1] == "hold" {
        unsafe {
            libc::setsid();
        }
        let mut f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&a[2])
            .unwrap();
        f.write_all(b"descendant").unwrap();
        f.sync_all().unwrap();
        fs::write(&a[3], b"ready").unwrap();
        std::thread::sleep(Duration::from_secs(30));
        return;
    }
    if a[1] == "as-user" || a[1] == "missing-device" {
        unsafe {
            assert_eq!(libc::setgroups(0, std::ptr::null()), 0);
            assert_eq!(libc::setgid(1500), 0);
            assert_eq!(libc::setuid(1500), 0);
        }
        use std::os::unix::process::CommandExt;
        let mode = if a[1] == "missing-device" {
            "missing-device-user"
        } else {
            "qualify-user"
        };
        panic!(
            "fresh UID1500 exec failed: {}",
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([mode, &a[2]])
                .exec()
        );
    }
    if unsafe { libc::geteuid() } != 0 {
        assert!(fs::read("/tmp/vfs-root-only/secret").is_err());
        assert!(fs::read_dir("/root").is_err());
        if let Ok(fd) = fs::File::open("/proc/1/ns/mnt") {
            assert_eq!(
                unsafe { libc::setns(fd.as_raw_fd(), libc::CLONE_NEWNS) },
                -1
            );
        }
    }
    let namespace = if unsafe { libc::geteuid() } == 0 {
        PrivateMountNamespace::enter()
    } else {
        PrivateMountNamespace::enter_for_current_user()
    }
    .unwrap();
    let s = LocalStore::open(root.join("cas")).unwrap();
    let m = VfsManager::open(root.join("manager"), s.clone()).unwrap();
    if a[1] == "crash" {
        let v = m.open_view(&a[3], &owner()).unwrap();
        let p = m.root().join("mounts").join(v.id());
        let _mounted = mount(v, &p, &namespace).unwrap();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(p.join("file"))
            .unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"FUSE-fsynced").unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::rename(p.join("file"), p.join("acknowledged")).unwrap();
        fs::File::open(&p).unwrap().sync_all().unwrap();
        fs::write(root.join("crash-ack"), b"namespace and fsync acknowledged").unwrap();
        std::process::abort();
    }
    if a[1] == "missing-device-user" {
        let blob = s.put_blob(b"body").unwrap();
        let base = dir(&s, vec![f(&blob, b"file")]);
        prepare(&m, &base).unwrap();
        let view = fork(&m, &Uuid::new_v4().to_string(), &base, owner()).unwrap();
        let path = m.root().join("mounts").join(view.id());
        assert!(mount(view, &path, &namespace).is_err());
        println!(
            "{}",
            serde_json::json!({"missing_device_fails_closed":true,"uid":unsafe{libc::geteuid()}})
        );
        return;
    }
    let mut results = vec![];
    let blob = s.put_blob(b"inherited payload").unwrap();
    let range_report = if ranges() {
        Some(qualify_ranges(&s, &m, &namespace))
    } else {
        None
    };

    for count in [1, 100, 10_000, 100_000] {
        eprintln!("phase=prepare count={count}");
        let start = Instant::now();
        let lower_entries = (0..count)
            .map(|i| f(&blob, format!("f{i:06}").as_bytes()))
            .collect::<Vec<_>>();
        emit_stage(
            "allocate_fixture_entries",
            count,
            start,
            serde_json::json!({}),
        );
        let start = Instant::now();
        let lower = dir(&s, lower_entries);
        emit_stage(
            "construct_lower_fixture",
            count,
            start,
            serde_json::json!({}),
        );
        let empty = dir(&s, vec![]);
        let base = dir(
            &s,
            vec![
                DirectoryEntry::new(b"tree", EntryNode::Directory { object: lower }).unwrap(),
                DirectoryEntry::new(b"dest", EntryNode::Directory { object: empty }).unwrap(),
                f(&blob, b"a"),
                f(&blob, b"equal"),
                f(&blob, b"file"),
            ],
        );
        let start = Instant::now();
        prepare(&m, &base).unwrap();
        let prepare = start.elapsed();
        emit_stage("prepare_retained_root", count, start, serde_json::json!({}));
        let mut fork_samples = Vec::new();
        let mut rename_samples = Vec::new();
        for trial_index in 0..5 {
            let trial_begin = Instant::now();
            let start = Instant::now();
            let trial = fork(&m, &Uuid::new_v4().to_string(), &base, owner()).unwrap();
            fork_samples.push(start.elapsed().as_nanos());
            assert_eq!(trial.descriptor().unwrap().inode_count, 1);
            let tp = m.root().join("mounts").join(trial.id());
            let mut tm = mount(trial.clone(), &tp, &namespace).unwrap();
            let start = Instant::now();
            fs::rename(tp.join("tree"), tp.join("dest/moved")).unwrap();
            rename_samples.push(start.elapsed().as_nanos());
            assert_eq!(trial.descriptor().unwrap().inode_count, 3);
            assert_eq!(fs::metadata(&tp).unwrap().nlink(), 3);
            assert_eq!(fs::metadata(tp.join("dest")).unwrap().nlink(), 3);
            tm.unmount().unwrap();
            emit_stage(
                "fork_mount_rename_trial",
                count,
                trial_begin,
                serde_json::json!({"trial":trial_index,"fork_ns":fork_samples.last(),"rename_ns":rename_samples.last(),"inodes":3}),
            );
        }
        let start = Instant::now();
        let v = fork(&m, &Uuid::new_v4().to_string(), &base, owner()).unwrap();
        let fork = start.elapsed();
        assert_eq!(v.descriptor().unwrap().inode_count, 1);
        let p = m.root().join("mounts").join(v.id());
        let mut mounted = mount(v.clone(), &p, &namespace).unwrap();
        eprintln!("phase=mounted count={count}");
        let start = Instant::now();
        fs::rename(p.join("tree"), p.join("dest/moved")).unwrap();
        let rename = start.elapsed();
        assert_eq!(fs::metadata(&p).unwrap().nlink(), 3);
        assert_eq!(fs::metadata(p.join("dest")).unwrap().nlink(), 3);
        assert_eq!(v.descriptor().unwrap().inode_count, 3);
        assert!(!p.join("tree").exists());
        assert_eq!(
            fs::read(p.join("dest/moved").join(format!("f{:06}", count - 1))).unwrap(),
            b"inherited payload"
        );
        if count > 1 {
            mounted.unmount().unwrap();
            results.push(scaling_result(serde_json::json!({"entries":count,"prepare_ns":prepare.as_nanos(),"fork_ns":fork.as_nanos(),"rename_syscall_ns":rename.as_nanos(),"inodes_after_rename":3,"fork_samples_ns":fork_samples,"rename_samples_ns":rename_samples})));
            continue;
        }
        eprintln!("phase=identity");
        assert_ne!(
            fs::metadata(p.join("a")).unwrap().ino(),
            fs::metadata(p.join("equal")).unwrap().ino()
        );
        let mut held = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(p.join("a"))
            .unwrap();
        fs::hard_link(p.join("a"), p.join("alias")).unwrap();
        assert_eq!(
            fs::metadata(p.join("a")).unwrap().ino(),
            fs::metadata(p.join("alias")).unwrap().ino()
        );
        assert_eq!(fs::metadata(p.join("alias")).unwrap().nlink(), 2);
        held.write_all(b"private").unwrap();
        held.sync_all().unwrap();
        assert!(fs::read(p.join("alias")).unwrap().starts_with(b"private"));
        assert_eq!(fs::read(p.join("equal")).unwrap(), b"inherited payload");
        fs::remove_file(p.join("a")).unwrap();
        fs::remove_file(p.join("alias")).unwrap();
        held.seek(SeekFrom::Start(0)).unwrap();
        let mut b = vec![];
        held.read_to_end(&mut b).unwrap();
        assert!(b.starts_with(b"private"));
        assert!(mounted.unmount().is_err());
        assert!(v.checkpoint_snapshot_unmounted().is_err());
        drop(held);
        eprintln!("phase=descendant");
        let ready = root.join("writer-ready");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "hold",
                p.join("file").to_str().unwrap(),
                ready.to_str().unwrap(),
            ])
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "descendant did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(mounted.unmount().is_err());
        child.kill().unwrap();
        child.wait().unwrap();
        eprintln!("phase=mmap");
        let mapped = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(p.join("file"))
            .unwrap();
        unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                17,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                mapped.as_raw_fd(),
                0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            std::ptr::copy_nonoverlapping(b"MAPPED".as_ptr(), ptr.cast::<u8>(), 6);
            assert_eq!(libc::msync(ptr, 17, libc::MS_SYNC), 0);
            assert_eq!(libc::munmap(ptr, 17), 0);
        }
        mapped.sync_all().unwrap();
        if ranges() {
            let nonce = Uuid::new_v4().to_string();
            let observed = v
                .observe_source(&Default::default(), &nonce, &Default::default())
                .unwrap();
            assert_eq!(
                observed.consistency,
                rudder_engine::vfs::ObservationConsistency::LiveBestEffort
            );
            assert!(v.checkpoint_snapshot_unmounted().is_err());
            v.release_observation(&nonce).unwrap();
        }
        drop(mapped);
        assert!(fs::read(p.join("file")).unwrap().starts_with(b"MAPPED"));
        fs::write(p.join("save.tmp"), b"atomic save").unwrap();
        fs::rename(p.join("save.tmp"), p.join("equal")).unwrap();
        assert_eq!(fs::read(p.join("equal")).unwrap(), b"atomic save");
        fs::set_permissions(p.join("equal"), fs::Permissions::from_mode(0o751)).unwrap();
        assert_eq!(
            fs::metadata(p.join("equal")).unwrap().mode() & 0o7777,
            0o751
        );
        let raw = std::ffi::OsString::from_vec(b"raw-\xff".to_vec());
        fs::write(p.join(&raw), b"raw bytes").unwrap();
        symlink(&raw, p.join("link")).unwrap();
        assert_eq!(fs::read_link(p.join("link")).unwrap(), PathBuf::from(&raw));
        assert_eq!(fs::read(p.join("link")).unwrap(), b"raw bytes");
        fs::hard_link(p.join("file"), p.join("file-alias")).unwrap();
        fs::File::open(&p).unwrap().sync_all().unwrap();
        eprintln!("phase=unmount");
        let view = mounted.unmount().unwrap();
        eprintln!("phase=checkpoint");
        let cp = view.checkpoint_snapshot_unmounted().unwrap();
        prepare_snapshot(&m, &cp).unwrap();
        eprintln!("phase=restore");
        let restored = fork_snapshot(&m, &Uuid::new_v4().to_string(), &cp, owner()).unwrap();
        let rp = m.root().join("mounts").join(restored.id());
        let mut remount = mount(restored.clone(), &rp, &namespace).unwrap();
        assert!(fs::read(rp.join("file")).unwrap().starts_with(b"MAPPED"));
        assert_eq!(
            fs::metadata(rp.join("file")).unwrap().ino(),
            fs::metadata(rp.join("file-alias")).unwrap().ino()
        );
        assert_eq!(
            fs::read(rp.join("dest/moved/f000000")).unwrap(),
            b"inherited payload"
        );
        remount.unmount().unwrap();
        results.push(scaling_result(serde_json::json!({"entries":count,"prepare_ns":prepare.as_nanos(),"fork_ns":fork.as_nanos(),"rename_syscall_ns":rename.as_nanos(),"inodes_after_rename":3,"fork_samples_ns":fork_samples,"rename_samples_ns":rename_samples,"mmap_msync":true,"open_fd_unlink":true,"hardlink_restore":true,"raw_names":true,"atomic_save":true,"strict_busy_unmount":true})));
    }
    eprintln!("phase=crash");
    let base = dir(&s, vec![f(&blob, b"file")]);
    prepare(&m, &base).unwrap();
    let id = Uuid::new_v4().to_string();
    drop(fork(&m, &id, &base, owner()).unwrap());
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["crash", root.to_str().unwrap(), &id])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        root.join("crash-ack").exists(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let recovered = m.open_view(&id, &owner()).unwrap();
    let inode = recovered.lookup(1, b"acknowledged").unwrap().ino;
    assert!(recovered
        .read(inode, 0, 100)
        .unwrap()
        .starts_with(b"FUSE-fsynced"));
    println!(
        "{}",
        serde_json::json!({"schema":1,"backend":"rudder-logical-fuser-0.18.0","kernel":fs::read_to_string("/proc/sys/kernel/osrelease").unwrap().trim(),"writeback_cache":false,"data_plane":if ranges(){"paged_ranges_v2"}else{"legacy_v1"},"range_report":range_report,"scaling":results,"server_abort_recovery":true,"detached_descendant_busy_then_reaped":true,"uid":unsafe{libc::geteuid()},"uid_map":fs::read_to_string("/proc/self/uid_map").unwrap().trim(),"gid_map":fs::read_to_string("/proc/self/gid_map").unwrap().trim(),"active_host_modified":false})
    );
}

fn qualify_ranges(
    s: &LocalStore,
    m: &VfsManager,
    namespace: &PrivateMountNamespace,
) -> serde_json::Value {
    use rudder_engine::content::{
        self, ContentStore, FileNode, StagedContentStore, CHUNK_BYTES, FILE_LAYOUT,
    };
    use rudder_engine::directory::{DirectoryEntry as Entry, EntryNode as Node};
    let stage = s.begin_staging(Duration::from_secs(600)).unwrap();
    let adapter = StagedContentStore::new(s, &stage.id).unwrap();
    let chunk = adapter.put_blob(&vec![b'q'; CHUNK_BYTES]).unwrap();
    let file = adapter
        .put_file_node(&FileNode {
            format_version: 1,
            layout: FILE_LAYOUT.into(),
            level: 0,
            logical_size: 52 * CHUNK_BYTES as u64,
            children: vec![chunk.clone(); 52],
        })
        .unwrap();
    s.publish(&stage.id, "mounted-large-file", &file).unwrap();
    let base = paged(
        s,
        vec![Entry::new(
            b"large",
            Node::File {
                object: file,
                logical_size: 52 * CHUNK_BYTES as u64,
                mode: 0o640,
            },
        )
        .unwrap()],
    );
    m.prepare_paged(&base).unwrap();
    let view = m
        .fork_paged(&Uuid::new_v4().to_string(), &base, owner())
        .unwrap();
    let sibling = m
        .fork_paged(&Uuid::new_v4().to_string(), &base, owner())
        .unwrap();
    let p = m.root().join("mounts").join(view.id());
    let sp = m.root().join("mounts").join(sibling.id());
    let mut mounted = mount(view.clone(), &p, namespace).unwrap();
    let mut sibling_mount = mount(sibling.clone(), &sp, namespace).unwrap();
    let log_bytes = || {
        fs::read_dir(m.root().join("views").join(view.id()).join("files"))
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum::<u64>()
    };
    assert_eq!(
        fs::metadata(p.join("large")).unwrap().len(),
        52 * CHUNK_BYTES as u64
    );
    let mut writable = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(p.join("large"))
        .unwrap();
    writable.seek(SeekFrom::Start(11)).unwrap();
    writable.write_all(b"changed").unwrap();
    writable.sync_all().unwrap();
    let first_copyup = log_bytes();
    assert_eq!(first_copyup, 64 * 1024);
    let mut original = fs::File::open(sp.join("large")).unwrap();
    original.seek(SeekFrom::Start(11)).unwrap();
    let mut bytes = [0; 7];
    original.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"qqqqqqq");
    drop(original);
    sibling_mount.unmount().unwrap();
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            writable.as_raw_fd(),
            (2 * CHUNK_BYTES) as libc::off_t,
        );
        assert_ne!(ptr, libc::MAP_FAILED);
        std::ptr::copy_nonoverlapping(b"MAPPED".as_ptr(), ptr.cast::<u8>(), 6);
        assert_eq!(libc::msync(ptr, 4096, libc::MS_SYNC), 0);
        assert_eq!(libc::munmap(ptr, 4096), 0);
    }
    writable.sync_all().unwrap();
    let nonce = Uuid::new_v4().to_string();
    let observation = view
        .observe_source(&Default::default(), &nonce, &Default::default())
        .unwrap();
    assert_eq!(
        observation.consistency,
        rudder_engine::vfs::ObservationConsistency::LiveBestEffort
    );
    assert!(view.checkpoint_snapshot_unmounted().is_err());
    assert!(log_bytes() <= 4 * 64 * 1024);
    let private_bytes = log_bytes();
    drop(writable);
    mounted.unmount().unwrap();
    let snapshot = view.checkpoint_snapshot_unmounted().unwrap();
    assert_eq!(snapshot.directory, observation.directory);
    view.release_observation(&nonce).unwrap();
    let Node::File {
        object,
        logical_size,
        ..
    } = rudder_engine::tree::lookup(s, &snapshot.directory, b"large")
        .unwrap()
        .unwrap()
        .node
    else {
        panic!()
    };
    let descriptor = content::decode_file_node(&s.read_object(&object).unwrap()).unwrap();
    let unchanged = descriptor
        .children
        .iter()
        .filter(|child| *child == &chunk)
        .count();
    assert_eq!(unchanged, 50);
    m.prepare_paged_snapshot(&snapshot).unwrap();
    let restored = m
        .fork_paged_snapshot(&Uuid::new_v4().to_string(), &snapshot, owner())
        .unwrap();
    let rp = m.root().join("mounts").join(restored.id());
    let mut remount = mount(restored, &rp, namespace).unwrap();
    let mut f = fs::File::open(rp.join("large")).unwrap();
    f.seek(SeekFrom::Start(11)).unwrap();
    f.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"changed");
    f.seek(SeekFrom::Start((2 * CHUNK_BYTES) as u64)).unwrap();
    let mut mapped = [0; 6];
    f.read_exact(&mut mapped).unwrap();
    assert_eq!(&mapped, b"MAPPED");
    drop(f);
    remount.unmount().unwrap();
    let result = serde_json::json!({"logical_bytes":logical_size,"first_write_private_bytes":first_copyup,"after_mmap_private_bytes":private_bytes,"unchanged_chunks":unchanged,"total_chunks":52,"sibling_kernel_isolation":true,"mmap_msync_restore":true,"live_observation_with_open_fd":true});
    println!(
        "{}",
        serde_json::json!({"event":"vfs_range_result","uid":unsafe{libc::geteuid()},"result":result})
    );
    std::io::stdout().flush().unwrap();
    result
}
