//! Generated-public-fixture qualification; Linux-only bin, never installed.
#[path = "remote_capture_qualification.rs"]
mod capture_qualification;
use anyhow::{bail, Context, Result};
use rudder_engine::{
    content::{FileNode, CHUNK_BYTES, FILE_LAYOUT},
    directory::{self, DirectoryEntry, DirectoryStore, EntryNode, PagedDirectory},
    protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef, OwnerEpoch},
    remote_cache::{CacheLimits, CacheScope, FetchControl, ImmutableObjectSource, RemoteCache},
    remote_root::{
        AttemptBinding, CurrentAttemptAuthority, RemoteRootRegistry, RemoteSession, RootBinding,
        RootLimits, RootReceiptSource,
    },
    remote_source::{
        NodeRemoteSource, NodeSourceConfig, ReplicatedDurability, ReplicatedRootReceipt,
        SourceError, SourceRequestControl, TransportProfile,
    },
    snapshot::{RunnableRoot, SourceV2},
    store::{ImportPolicy, LocalStore},
    vfs::{
        self,
        linux::{self, PrivateMountNamespace},
        ObservationBudget, RemoteOptions, VfsManager, VfsView,
    },
    workspace::WorkspaceProfile,
};
use std::os::unix::process::CommandExt;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

struct Authority(AtomicBool);
impl CurrentAttemptAuthority for Authority {
    fn validate(&self, _attempt: &AttemptBinding, _root: &RootBinding) -> Result<()> {
        if self.0.load(Ordering::Acquire) {
            bail!("private authority fence")
        };
        Ok(())
    }
}
struct Source {
    scope: CacheScope,
    objects: Mutex<BTreeMap<ObjectRef, Vec<u8>>>,
    root: Mutex<Option<ReplicatedRootReceipt>>,
    calls: Mutex<Vec<ObjectRef>>,
    root_calls: AtomicUsize,
    fail: AtomicBool,
}
impl Source {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            scope: CacheScope {
                provider_id: "vfs-remote-fixture".into(),
                tenant_id: Uuid::new_v4().to_string(),
            },
            objects: Mutex::new(BTreeMap::new()),
            root: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            root_calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        })
    }
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> ObjectRef {
        let reference = ObjectRef::from_bytes(kind, bytes);
        self.objects
            .lock()
            .unwrap()
            .insert(reference.clone(), bytes.to_vec());
        reference
    }
    fn metadata<T: serde::Serialize>(&self, kind: ObjectKind, value: &T) -> ObjectRef {
        self.put(kind, &canonical_json(value).unwrap())
    }
    fn publish_fixture(&self, root: &ObjectRef) {
        let objects = self.objects.lock().unwrap();
        *self.root.lock().unwrap() = Some(ReplicatedRootReceipt {
            format_version: 1,
            root_id: "prepared".into(),
            root: root.clone(),
            durability: ReplicatedDurability::Replicated,
            published_at_ms: 1,
            object_count: objects.len() as u64,
            total_bytes: objects.values().map(|value| value.len() as u64).sum(),
        });
    }
}
impl DirectoryStore for Source {
    fn read(&self, r: &ObjectRef) -> Result<Vec<u8>> {
        self.objects
            .lock()
            .unwrap()
            .get(r)
            .cloned()
            .context("missing fixture object")
    }
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectRef> {
        directory::references_from_bytes(kind, bytes)?;
        Ok(Source::put(self, kind, bytes))
    }
}
impl ImmutableObjectSource for Source {
    fn scope(&self) -> CacheScope {
        self.scope.clone()
    }
    fn fetch(
        &self,
        reference: &ObjectRef,
        control: &FetchControl,
        output: &mut dyn Write,
    ) -> Result<()> {
        self.calls.lock().unwrap().push(reference.clone());
        if self.fail.load(Ordering::Acquire) {
            bail!("provider unavailable")
        };
        let bytes = DirectoryStore::read(self, reference)?;
        for part in bytes.chunks(65536) {
            control.check()?;
            output.write_all(part)?;
        }
        Ok(())
    }
}
impl RootReceiptSource for Source {
    fn scope(&self) -> CacheScope {
        self.scope.clone()
    }
    fn get_root(
        &self,
        _id: &str,
        _control: &SourceRequestControl,
    ) -> std::result::Result<ReplicatedRootReceipt, SourceError> {
        self.root_calls.fetch_add(1, Ordering::AcqRel);
        if self.fail.load(Ordering::Acquire) {
            return Err(SourceError::RemoteFailed);
        }
        self.root
            .lock()
            .unwrap()
            .clone()
            .ok_or(SourceError::RemoteFailed)
    }
}
struct Graph {
    source: Arc<Source>,
    root: ObjectRef,
    directory: ObjectRef,
    file: ObjectRef,
    chunks: Vec<ObjectRef>,
    large_name: Vec<u8>,
}
fn wrap(source: &Source, directory: &ObjectRef, topology: serde_json::Value) -> ObjectRef {
    let policy = source.put(ObjectKind::Blob, b"{}");
    let profile = source.metadata(ObjectKind::Blob, &WorkspaceProfile::source_only());
    let hardlinks = source.metadata(ObjectKind::Blob, &topology);
    let source_root = source.metadata(
        ObjectKind::SourceRoot,
        &SourceV2 {
            format_version: 2,
            consistency: CheckpointConsistency::Quiesced,
            directory: directory.clone(),
            projection_policy: policy,
        },
    );
    source.metadata(
        ObjectKind::RunnableRoot,
        &RunnableRoot {
            format_version: 1,
            consistency: CheckpointConsistency::Quiesced,
            source: source_root,
            environment: None,
            view_root: directory.clone(),
            profile,
            hardlinks,
            knowledge_cursor: u64::MAX,
        },
    )
}
fn graph(count: usize) -> Graph {
    let source = Source::new();
    let chunks = vec![
        source.put(ObjectKind::Blob, &vec![b'q'; CHUNK_BYTES]),
        source.put(ObjectKind::Blob, &vec![b'r'; CHUNK_BYTES]),
    ];
    let mut children = vec![chunks[0].clone(); 52];
    children[1] = chunks[1].clone();
    let file = source.metadata(
        ObjectKind::File,
        &FileNode {
            format_version: 1,
            layout: FILE_LAYOUT.into(),
            level: 0,
            logical_size: 52 * CHUNK_BYTES as u64,
            children,
        },
    );
    let nested_data = source.put(ObjectKind::Blob, b"nested");
    let nested = PagedDirectory::new(source.as_ref())
        .build(
            0o751,
            vec![DirectoryEntry::new(
                b"child",
                EntryNode::File {
                    object: nested_data.clone(),
                    logical_size: nested_data.size_bytes,
                    mode: 0o640,
                },
            )
            .unwrap()],
        )
        .unwrap();
    let mut entries = Vec::new();
    for index in 0..count {
        entries.push(
            DirectoryEntry::new(
                format!("file-{index:08}").as_bytes(),
                EntryNode::File {
                    object: file.clone(),
                    logical_size: 52 * CHUNK_BYTES as u64,
                    mode: 0o751,
                },
            )
            .unwrap(),
        );
    }
    entries.push(DirectoryEntry::new(b"dir", EntryNode::Directory { object: nested }).unwrap());
    entries.sort_by_key(|entry| entry.name().unwrap());
    let directory = PagedDirectory::new(source.as_ref())
        .build(0o755, entries)
        .unwrap();
    let root = wrap(
        &source,
        &directory,
        serde_json::json!({"format_version":1,"groups":[]}),
    );
    source.publish_fixture(&root);
    Graph {
        source,
        root,
        directory,
        file,
        chunks,
        large_name: format!("file-{:08}", count / 2).into_bytes(),
    }
}
fn attempt() -> AttemptBinding {
    AttemptBinding {
        project_id: "project".into(),
        run_id: Uuid::new_v4().to_string(),
        run_epoch: 1,
        owner: OwnerEpoch {
            owner: "remote-owner".into(),
            epoch: 7,
        },
        execution_id: Uuid::new_v4().to_string(),
        operation_id: Uuid::new_v4().to_string(),
        view_id: Uuid::new_v4().to_string(),
    }
}

struct Http {
    url: String,
    roots: Arc<AtomicUsize>,
    objects: Arc<Mutex<Vec<ObjectRef>>>,
    stop: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
}
impl Http {
    fn new(source: Arc<Source>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let roots = Arc::new(AtomicUsize::new(0));
        let objects = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let listener = listener.try_clone().unwrap();
            let roots = roots.clone();
            let objects = objects.clone();
            let stop = stop.clone();
            let source = source.clone();
            workers.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_read_timeout(Some(Duration::from_millis(50)))
                                .unwrap();
                            stream
                                .set_write_timeout(Some(Duration::from_secs(5)))
                                .unwrap();
                            let mut bytes = Vec::new();
                            let mut part = [0; 1024];
                            while bytes.len() <= 16384
                                && !bytes.windows(4).any(|v| v == b"\r\n\r\n")
                                && !stop.load(Ordering::Acquire)
                            {
                                match stream.read(&mut part) {
                                    Ok(0) => break,
                                    Ok(n) => bytes.extend_from_slice(&part[..n]),
                                    Err(error)
                                        if matches!(
                                            error.kind(),
                                            std::io::ErrorKind::WouldBlock
                                                | std::io::ErrorKind::TimedOut
                                        ) => {}
                                    Err(_) => break,
                                }
                            }
                            let request = String::from_utf8_lossy(&bytes);
                            assert!(request
                                .to_ascii_lowercase()
                                .contains("authorization: bearer public-root-fixture\r\n"));
                            let path = request
                                .lines()
                                .next()
                                .unwrap_or("")
                                .split(' ')
                                .nth(1)
                                .unwrap_or("");
                            let response = if path == "/api/rudder/fabric/roots/prepared" {
                                roots.fetch_add(1, Ordering::AcqRel);
                                Some(
                                    serde_json::to_vec(
                                        source.root.lock().unwrap().as_ref().unwrap(),
                                    )
                                    .unwrap(),
                                )
                            } else if let Some(suffix) =
                                path.strip_prefix("/api/rudder/fabric/objects/")
                            {
                                let pieces: Vec<_> = suffix.split('/').collect();
                                if pieces.len() != 3 {
                                    None
                                } else {
                                    let kind: ObjectKind = serde_json::from_value(
                                        serde_json::Value::String(pieces[0].into()),
                                    )
                                    .unwrap();
                                    let reference = ObjectRef {
                                        kind,
                                        algorithm: rudder_engine::protocol::HashAlgorithm::Sha256,
                                        digest: pieces[1].into(),
                                        size_bytes: pieces[2].parse().unwrap(),
                                    };
                                    objects.lock().unwrap().push(reference.clone());
                                    source.objects.lock().unwrap().get(&reference).cloned()
                                }
                            } else {
                                None
                            };
                            let header = format!(
                                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                if response.is_some() {
                                    "200 OK"
                                } else {
                                    "404 Not Found"
                                },
                                response.as_ref().map_or(0, Vec::len)
                            );
                            let _ = stream.write_all(header.as_bytes());
                            if let Some(body) = response {
                                let _ = stream.write_all(&body);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2))
                        }
                        Err(_) => return,
                    }
                }
            }));
        }
        Self {
            url: format!(
                "http://{}/api/rudder/fabric",
                listener.local_addr().unwrap()
            ),
            roots,
            objects,
            stop,
            workers,
        }
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            worker.join().unwrap();
        }
    }
}

fn emit(value: serde_json::Value) {
    println!("{value}");
    let _ = std::io::stdout().flush();
}
fn until(mut check: impl FnMut() -> bool) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(10);
    while !check() {
        if Instant::now() >= end {
            bail!("qualification cleanup deadline")
        };
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}
fn private(path: &Path) -> Result<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn log_bytes(manager: &VfsManager, view: &VfsView) -> Result<u64> {
    Ok(
        fs::read_dir(manager.root().join("views").join(view.id()).join("files"))?
            .map(|entry| entry.and_then(|entry| entry.metadata()).map(|m| m.len()))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .sum(),
    )
}
fn admitted(registry: &RemoteRootRegistry, graph: &Graph) -> Result<RemoteSession> {
    Ok(registry.admit(
        &attempt(),
        "prepared",
        &graph.root,
        &SourceRequestControl::new(Instant::now() + Duration::from_secs(30)),
    )?)
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct DirRecord {
    name: Vec<u8>,
    ino: u64,
    cookie: i64,
    kind: u8,
}
fn getdents(file: &fs::File, bytes: usize) -> Result<Vec<DirRecord>> {
    use std::os::fd::AsRawFd;
    anyhow::ensure!(
        (512..=32768).contains(&bytes),
        "invalid getdents fixture buffer"
    );
    let mut data = vec![0u8; bytes];
    let n = unsafe {
        libc::syscall(
            libc::SYS_getdents64,
            file.as_raw_fd(),
            data.as_mut_ptr(),
            data.len(),
        )
    };
    if n < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut offset = 0;
    let n = n as usize;
    let mut output = Vec::new();
    while offset < n {
        anyhow::ensure!(offset + 19 <= n, "short directory record");
        let ino = u64::from_ne_bytes(data[offset..offset + 8].try_into()?);
        let cookie = i64::from_ne_bytes(data[offset + 8..offset + 16].try_into()?);
        let length = u16::from_ne_bytes(data[offset + 16..offset + 18].try_into()?) as usize;
        anyhow::ensure!(
            length >= 20 && offset + length <= n,
            "invalid directory record length"
        );
        let raw = &data[offset + 19..offset + length];
        let end = raw
            .iter()
            .position(|v| *v == 0)
            .context("directory name terminator")?;
        output.push(DirRecord {
            name: raw[..end].to_vec(),
            ino,
            cookie,
            kind: data[offset + 18],
        });
        offset += length;
    }
    Ok(output)
}
fn seek_directory(file: &fs::File, cookie: i64) -> Result<()> {
    use std::os::fd::AsRawFd;
    anyhow::ensure!(
        unsafe { libc::lseek(file.as_raw_fd(), cookie, libc::SEEK_SET) } == cookie,
        "directory seek failed"
    );
    Ok(())
}
fn scan_directory(file: &fs::File, expected: usize, old: bool) -> Result<(Vec<DirRecord>, usize)> {
    let mut count = 0;
    let mut previous = None;
    let mut witness = Vec::new();
    let mut calls = 0;
    let mut old_a = false;
    let mut old_b = false;
    let mut new_a = false;
    let mut new_b = false;
    loop {
        // The first tiny buffer exercises a kernel reply ending mid-page.
        let rows = getdents(file, if calls < 2 { 512 } else { 32768 })?;
        calls += 1;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            if row.name == b"." || row.name == b".." {
                continue;
            }
            anyhow::ensure!(
                row.ino > 1 && row.cookie > 2 && matches!(row.kind, libc::DT_REG | libc::DT_DIR),
                "unverified/incomplete listing record"
            );
            if let Some(last) = &previous {
                anyhow::ensure!(last < &row.name, "directory listing order changed");
            }
            previous = Some(row.name.clone());
            count += 1;
            old_a |= row.name == b"file-00000000";
            old_b |= row.name == b"file-00000002";
            new_a |= row.name == b"readdir-renamed";
            new_b |= row.name == b"readdir-added";
            if witness.len() < 4 {
                witness.push(row);
            }
        }
    }
    anyhow::ensure!(
        count == expected,
        "directory count differs: {count} vs {expected}"
    );
    anyhow::ensure!(
        old_a == old && old_b == old && new_a != old && new_b != old,
        "fixed directory snapshot changed"
    );
    Ok((witness, calls))
}
fn qualify_readdir(
    mount: &Path,
    manager: &VfsManager,
    view: &VfsView,
    http: &Http,
    expected: usize,
) -> Result<serde_json::Value> {
    let before = http.objects.lock().unwrap().len();
    let started = Instant::now();
    let file = fs::File::open(mount)?;
    let (witness, calls) = scan_directory(&file, expected, true)?;
    let listing_ms = started.elapsed().as_millis();
    let stages = view.readdir_stats()?.context("missing cursor stats")?;
    anyhow::ensure!(
        view.descriptor()?.inode_count == 1,
        "readdir materialized child attributes"
    );
    anyhow::ensure!(
        http.objects.lock().unwrap()[before..]
            .iter()
            .all(|r| r.kind == ObjectKind::DirectoryShard),
        "listing hydrated File descriptors or Blobs"
    );
    let metadata_requests = http.objects.lock().unwrap().len() - before;
    seek_directory(&file, witness[0].cookie)?;
    anyhow::ensure!(
        getdents(&file, 512)?.first() == Some(&witness[1]),
        "mid-page cookie changed"
    );
    // Change the namespace while the old kernel handle remains open.
    fs::rename(mount.join("file-00000000"), mount.join("readdir-renamed"))?;
    fs::remove_file(mount.join("file-00000002"))?;
    drop(fs::File::create(mount.join("readdir-added"))?);
    seek_directory(&file, 0)?;
    scan_directory(&file, expected, true)?;
    drop(file);
    until(|| {
        view.readdir_stats()
            .is_ok_and(|s| s.is_some_and(|s| s.live_cursors == 0))
    })?;
    let next = fs::File::open(mount)?;
    scan_directory(&next, expected, false)?;
    drop(next);
    until(|| {
        view.readdir_stats()
            .is_ok_and(|s| s.is_some_and(|s| s.live_cursors == 0))
    })?;
    let after = view.readdir_stats()?.context("cursor stats")?;
    anyhow::ensure!(
        after.live_cursors == 0 && after.cookies == 0 && after.overlay_rows == 0,
        "cursor quota was not released"
    );
    anyhow::ensure!(
        manager.store().retention()?.pins.is_empty(),
        "directory listing minted complete pin"
    );
    Ok(
        serde_json::json!({"entries":expected,"listing_ms":listing_ms,"getdents_calls":calls,"metadata_requests":metadata_requests,"file_or_blob_requests_during_listing":0,"stages":stages,"stable_seek_rewind":true,"namespace_mutation_fixed_snapshot":true,"close_reopen_current_snapshot":true,"mid_page_cookie":true,"kernel_cursor_cleanup":true}),
    )
}
fn exercise(base: &Path, helper: &Path, node: &Path, mode: &str) -> Result<()> {
    anyhow::ensure!(
        base.is_absolute() && helper.is_absolute() && node.is_absolute(),
        "absolute fixture inputs required"
    );
    anyhow::ensure!(
        fs::read_dir(base)?.next().is_none(),
        "qualification base must be empty"
    );
    let before = match fs::read_link("/proc/1/ns/mnt") {
        Ok(before) => Some(before),
        Err(error)
            if mode == "as-user"
                && unsafe { libc::geteuid() } == 1500
                && error.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    let namespace = if mode == "as-user" {
        anyhow::ensure!(
            unsafe { libc::geteuid() } == 1500,
            "rootless qualifier requires fresh UID1500 executable"
        );
        PrivateMountNamespace::enter_for_current_user()?
    } else {
        PrivateMountNamespace::enter()?
    };
    let uid = unsafe { libc::geteuid() };
    emit(serde_json::json!({"phase":"fixture","uid":uid,"entries":100101}));
    let start = Instant::now();
    let graph = graph(100100);
    let fixture_ms = start.elapsed().as_millis();
    let http = Http::new(graph.source.clone());
    let mut config = NodeSourceConfig::new(
        graph.source.scope.clone(),
        http.url.clone(),
        BTreeMap::from([("Authorization".into(), "Bearer public-root-fixture".into())]),
    );
    config.transport = TransportProfile::LoopbackHttpTest;
    let source = Arc::new(NodeRemoteSource::spawn(node, helper, config)?);
    private(&base.join("cache"))?;
    private(&base.join("registry"))?;
    let store = LocalStore::open(base.join("local-cas"))?;
    let cache = RemoteCache::open(
        base.join("cache"),
        source.clone(),
        CacheLimits {
            max_resident_bytes: 64 * 1024 * 1024,
            max_borrowed_bytes: 64 * 1024 * 1024,
            ..Default::default()
        },
    )?;
    let authority = Arc::new(Authority(AtomicBool::new(false)));
    let registry = RemoteRootRegistry::open(
        base.join("registry"),
        cache.clone(),
        source.clone(),
        authority.clone(),
        RootLimits {
            max_borrowed_bytes: 64 * 1024 * 1024,
            ..Default::default()
        },
    )?;
    let first_session = admitted(&registry, &graph)?;
    anyhow::ensure!(
        http.objects.lock().unwrap().is_empty(),
        "root admission fetched tree bytes"
    );
    let started = Instant::now();
    let manager = VfsManager::open_remote(
        base.join("manager"),
        store.clone(),
        first_session.clone(),
        &graph.root,
        RemoteOptions::default(),
    )?;
    let view = manager.fork_remote()?;
    let launch_ms = started.elapsed().as_millis();
    anyhow::ensure!(
        view.descriptor()?.inode_count == 1,
        "remote fork walked inherited entries"
    );
    anyhow::ensure!(
        store.retention()?.pins.is_empty(),
        "remote fork minted complete pin"
    );
    anyhow::ensure!(
        store.read_object(&graph.directory).is_err() && store.read_object(&graph.file).is_err(),
        "remote inherited objects imported into local CAS"
    );
    anyhow::ensure!(
        http.objects
            .lock()
            .unwrap()
            .iter()
            .all(|r| !graph.chunks.contains(r)),
        "startup hydrated payload"
    );
    let second_session = admitted(&registry, &graph)?;
    let sibling_manager = VfsManager::open_remote(
        manager.root(),
        store.clone(),
        second_session.clone(),
        &graph.root,
        RemoteOptions::default(),
    )?;
    let sibling = sibling_manager.fork_remote()?;
    let mut mounted = linux::mount(
        view.clone(),
        manager.root().join("mounts").join(view.id()),
        &namespace,
    )?;
    let mut sibling_mount = linux::mount(
        sibling.clone(),
        manager.root().join("mounts").join(sibling.id()),
        &namespace,
    )?;
    let readdir = qualify_readdir(mounted.path(), &manager, &view, &http, 100101)?;
    emit(serde_json::json!({"phase":"mounted_readdir","uid":uid,"results":readdir}));
    let name = std::ffi::OsString::from(String::from_utf8(graph.large_name.clone())?);
    let path = mounted.path().join(&name);
    let original = sibling_mount.path().join(&name);
    let file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
    anyhow::ensure!(
        file.metadata()?.len() == 208 * 1024 * 1024,
        "descriptor size used as file length"
    );
    let mut bytes = [0; 7];
    file.read_exact_at(&mut bytes, 11)?;
    anyhow::ensure!(&bytes == b"qqqqqqq", "cold range mismatch");
    file.write_all_at(b"changed", 11)?;
    file.sync_all()?;
    anyhow::ensure!(
        log_bytes(&manager, &view)? == 65536,
        "first write copied inherited file"
    );
    let sibling_file = fs::OpenOptions::new().read(true).open(original)?;
    sibling_file.read_exact_at(&mut bytes, 11)?;
    anyhow::ensure!(&bytes == b"qqqqqqq", "sibling inherited bytes changed");
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            std::os::fd::AsRawFd::as_raw_fd(&file),
            CHUNK_BYTES as libc::off_t,
        )
    };
    anyhow::ensure!(mapped != libc::MAP_FAILED, "mmap failed");
    unsafe {
        std::ptr::copy_nonoverlapping(b"MMAP".as_ptr(), mapped.cast::<u8>().add(5), 4);
        anyhow::ensure!(
            libc::msync(mapped, page, libc::MS_SYNC) == 0,
            "msync failed"
        );
        anyhow::ensure!(libc::munmap(mapped, page) == 0, "munmap failed")
    };
    file.sync_all()?;
    anyhow::ensure!(
        log_bytes(&manager, &view)? == 131072,
        "mmap did not remain bounded64KiB CoW"
    );
    let mut mapped_bytes = [0; 4];
    file.read_exact_at(&mut mapped_bytes, CHUNK_BYTES as u64 + 5)?;
    anyhow::ensure!(&mapped_bytes == b"MMAP", "mmap marker was not observed");
    sibling_file.read_exact_at(&mut mapped_bytes, CHUNK_BYTES as u64 + 5)?;
    anyhow::ensure!(&mapped_bytes == b"rrrr", "mmap changed sibling content");
    fs::rename(mounted.path().join("dir"), mounted.path().join("renamed"))?;
    anyhow::ensure!(
        fs::read(mounted.path().join("renamed/child"))? == b"nested",
        "immutable origin lost after inherited directory rename"
    );
    fs::create_dir(mounted.path().join("private"))?;
    fs::rename(
        mounted.path().join("renamed/child"),
        mounted.path().join("private/moved"),
    )?;
    fs::hard_link(
        mounted.path().join("private/moved"),
        mounted.path().join("linked"),
    )?;
    anyhow::ensure!(
        fs::metadata(mounted.path().join("private/moved"))?.ino()
            == fs::metadata(mounted.path().join("linked"))?.ino(),
        "hardlink inode identity differs"
    );
    fs::remove_file(mounted.path().join("private/moved"))?;
    let unlinked = fs::File::open(mounted.path().join("linked"))?;
    fs::remove_file(mounted.path().join("linked"))?;
    let mut bytes = [0; 6];
    unlinked.read_exact_at(&mut bytes, 0)?;
    anyhow::ensure!(&bytes == b"nested", "open FD lost after unlink");
    drop(unlinked);
    let payload_requests = http
        .objects
        .lock()
        .unwrap()
        .iter()
        .filter(|r| graph.chunks.contains(r))
        .cloned()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        payload_requests.len() == 2,
        "full file hydrated or unchanged chunk refetched"
    );
    anyhow::ensure!(
        cache.stats()?.disk_bytes < 64 * 1024 * 1024,
        "unexpected SDK hydration"
    );
    let (capture, capture_receipt) = capture_qualification::qualify_capture(
        &manager,
        &view,
        &first_session,
        &graph,
        &file,
        &sibling_file,
    )?;
    emit(serde_json::json!({"phase":"mounted_partial_capture","uid":uid,"results":capture}));
    anyhow::ensure!(
        http.objects
            .lock()
            .unwrap()
            .iter()
            .filter(|r| graph.chunks.contains(r))
            .count()
            == 2,
        "capture hydrated additional unchanged SDK chunks"
    );
    let busy = mounted
        .unmount()
        .err()
        .context("open handles unexpectedly allowed strict unmount")?;
    anyhow::ensure!(
        vfs::errno(&busy) == libc::EBUSY,
        "busy unmount returned another error"
    );
    drop(file);
    drop(sibling_file);
    let detached = mounted.unmount()?;
    sibling_mount.unmount()?;
    anyhow::ensure!(
        vfs::errno(&detached.checkpoint_snapshot_unmounted().unwrap_err()) == libc::EOPNOTSUPP,
        "partial checkpoint was accepted"
    );
    anyhow::ensure!(
        vfs::errno(
            &detached
                .observe_source(
                    &ImportPolicy::default(),
                    &Uuid::new_v4().to_string(),
                    &ObservationBudget::default()
                )
                .unwrap_err()
        ) == libc::EOPNOTSUPP,
        "partial observation was accepted"
    );
    let mut remounted = linux::mount(
        view.clone(),
        manager.root().join("mounts").join(view.id()),
        &namespace,
    )?;
    authority.0.store(true, Ordering::Release);
    first_session.invalidate();
    second_session.invalidate();
    let stale = fs::metadata(remounted.path().join("file-00000001"))
        .err()
        .context("stale attempt performed a new kernel lookup")?;
    anyhow::ensure!(
        stale.raw_os_error() == Some(libc::ESTALE),
        "stale lookup failed for an unexpected reason"
    );
    remounted.discard()?;
    let retained_after_revoke =
        rudder_engine::delta::read_partial_tree(&store, &capture_receipt.retention)?;
    anyhow::ensure!(
        retained_after_revoke.capture_id == capture_receipt.request_nonce
            && view
                .inspect_partial_capture(&capture_receipt.request_nonce)
                .is_err(),
        "saved data receipt restored live attempt authority"
    );
    anyhow::ensure!(
        vfs::errno(&view.read(1, 0, 1).unwrap_err()) == libc::ESTALE,
        "fenced logical operation succeeded"
    );
    anyhow::ensure!(
        vfs::errno(&view.checkpoint_snapshot_unmounted().unwrap_err()) == libc::EOPNOTSUPP,
        "discard forged partial checkpoint"
    );
    source.terminate();
    until(|| source.stats().is_ok_and(|stats| stats.child_exited))?;
    let host_namespace_read = fs::read_link("/proc/1/ns/mnt");
    let host_namespace_verified = match &host_namespace_read {
        Ok(after) => {
            anyhow::ensure!(
                before.as_ref() == Some(after),
                "initial host mount namespace changed"
            );
            true
        }
        Err(error) => {
            anyhow::ensure!(
                uid == 1500 && error.kind() == std::io::ErrorKind::PermissionDenied,
                "unexpected host namespace inspection failure"
            );
            false
        }
    };
    emit(
        serde_json::json!({"ok":true,"profile":"remote_partial_capture_v1","build_source_sha256":option_env!("RUDDER_QUALIFICATION_SOURCE_SHA").unwrap_or("unrecorded"),"uid":uid,"kernel":fs::read_to_string("/proc/sys/kernel/osrelease")?.trim(),"entries":100101,"logical_file_bytes":218103808u64,"fixture_ms":fixture_ms,"constructor_and_fork_ms":launch_ms,"readdir":readdir,"partial_capture":capture,"retained_data_after_revocation":true,"source_transport":"persistent Node helper + loopback authenticated HTTP fixture","root_receipt_requests":http.roots.load(Ordering::Acquire),"source_object_requests":http.objects.lock().unwrap().len(),"source_payload_requests":payload_requests.len(),"source_payload_bytes":payload_requests.iter().map(|r|r.size_bytes).sum::<u64>(),"partial_cache_bytes":cache.stats()?.disk_bytes,"private_copyup_bytes":log_bytes(&manager,&view)?,"complete_local_pins":store.retention()?.pins.len(),"mmap_msync_fsync":true,"sibling_isolation":true,"origin_rename":true,"runtime_hardlinks":true,"open_unlink":true,"busy_unmount_retry":true,"checkpoint_and_observation_rejected":true,"stale_lookup_rejected":true,"discard_strictly_closed":true,"discard_checkpoint_proof":false,"initial_host_mount_namespace_unchanged":host_namespace_verified,"host_namespace_check_delegated_to_outer_controller":!host_namespace_verified,"host_namespace_inspection_denied":host_namespace_read.is_err()}),
    );
    drop((
        remounted,
        mounted,
        sibling_mount,
        detached,
        view,
        sibling,
        manager,
        sibling_manager,
        first_session,
        second_session,
        registry,
        cache,
        source,
        http,
        store,
    ));
    fs::remove_dir_all(base)?;
    anyhow::ensure!(!base.exists(), "owned fixture cleanup failed");
    emit(serde_json::json!({"owned_fixture_removed":true,"uid":uid}));
    Ok(())
}
pub fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("--build-info") {
        emit(
            serde_json::json!({"format_version":1,"qualification":"remote_partial_capture_v1","ready":true,"default":false,"build_source_sha256":option_env!("RUDDER_QUALIFICATION_SOURCE_SHA").unwrap_or("unrecorded")}),
        );
        return;
    }
    if arguments.first().map(String::as_str) == Some("as-user") && unsafe { libc::geteuid() } == 0 {
        // A credential change resets dumpability/proc-map ownership. A fresh
        // exec restores self-owned /proc map files before entering UserNS.
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0
            || unsafe { libc::setresgid(1500, 1500, 1500) } != 0
            || unsafe { libc::setresuid(1500, 1500, 1500) } != 0
        {
            emit(serde_json::json!({"ok":false,"error":"drop fixture credentials failed"}));
            std::process::exit(1);
        }
        let failure = Command::new(std::env::current_exe().expect("qualifier executable"))
            .args(&arguments)
            .exec();
        emit(
            serde_json::json!({"ok":false,"error":"fresh UID1500 exec failed","errno":failure.raw_os_error()}),
        );
        std::process::exit(1);
    }
    let result = (|| -> Result<()> {
        anyhow::ensure!(
            arguments.len() == 4,
            "usage: qualifier qualify|as-user OWN_EMPTY_DIR HELPER_SCRIPT NODE"
        );
        anyhow::ensure!(
            matches!(arguments[0].as_str(), "qualify" | "as-user"),
            "unsupported mode"
        );
        exercise(
            Path::new(&arguments[1]),
            Path::new(&arguments[2]),
            Path::new(&arguments[3]),
            &arguments[0],
        )
    })();
    if result.is_err() {
        emit(
            serde_json::json!({"ok":false,"error":"remote mounted qualification failed","detail":format!("{:#}",result.unwrap_err())}),
        );
        std::process::exit(1);
    }
}
