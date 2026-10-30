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
        ReplicatedDurability, ReplicatedRootReceipt, SourceError, SourceRequestControl,
    },
    snapshot::{RunnableRoot, SourceV2},
    store::{ImportPolicy, LocalStore},
    vfs::{self, NodeKind, ObservationBudget, RemoteOptions, VfsManager, VfsView},
    workspace::WorkspaceProfile,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;
use uuid::Uuid;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|error| error.into_inner())
}
fn temp() -> TempDir {
    let base = std::env::var_os("RUDDER_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if Path::new("/workspace").is_dir() {
                PathBuf::from("/workspace")
            } else {
                std::env::temp_dir()
            }
        });
    let result = tempfile::tempdir_in(base).unwrap();
    fs::set_permissions(result.path(), fs::Permissions::from_mode(0o700)).unwrap();
    result
}
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
fn session(registry: &RemoteRootRegistry, graph: &Graph) -> RemoteSession {
    registry
        .admit(
            &attempt(),
            "prepared",
            &graph.root,
            &SourceRequestControl::new(Instant::now() + Duration::from_secs(5)),
        )
        .unwrap()
}
struct Rig {
    _dir: TempDir,
    store: LocalStore,
    cache: RemoteCache,
    registry: RemoteRootRegistry,
    authority: Arc<Authority>,
    session: RemoteSession,
    manager: VfsManager,
    view: VfsView,
}
fn rig(graph: &Graph, options: RemoteOptions) -> Rig {
    let dir = temp();
    for name in ["cache", "registry"] {
        fs::create_dir(dir.path().join(name)).unwrap();
        fs::set_permissions(dir.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let store = LocalStore::open(dir.path().join("local-cas")).unwrap();
    let cache = RemoteCache::open(
        dir.path().join("cache"),
        graph.source.clone(),
        CacheLimits {
            max_resident_bytes: 64 * 1024 * 1024,
            max_borrowed_bytes: 64 * 1024 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let authority = Arc::new(Authority(AtomicBool::new(false)));
    let registry = RemoteRootRegistry::open(
        dir.path().join("registry"),
        cache.clone(),
        graph.source.clone(),
        authority.clone(),
        RootLimits {
            max_borrowed_bytes: 64 * 1024 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let session = session(&registry, graph);
    let manager = VfsManager::open_remote(
        dir.path().join("manager"),
        store.clone(),
        session.clone(),
        &graph.root,
        options,
    )
    .unwrap();
    let view = manager.fork_remote().unwrap();
    Rig {
        _dir: dir,
        store,
        cache,
        registry,
        authority,
        session,
        manager,
        view,
    }
}
fn private_bytes(manager: &VfsManager, view: &VfsView) -> u64 {
    fs::read_dir(manager.root().join("views").join(view.id()).join("files"))
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum()
}

#[test]
fn remote_100k_fork_lookup_and_208mib_ranges_never_hydrate_or_pin_the_sdk() {
    let _serial = serial();
    let start = Instant::now();
    let graph = graph(99999);
    let fixture_ms = start.elapsed().as_millis();
    let start = Instant::now();
    let rig = rig(&graph, RemoteOptions::default());
    let launch_ms = start.elapsed().as_millis();
    assert_eq!(rig.view.descriptor().unwrap().inode_count, 1);
    assert_eq!(
        rig.view.descriptor().unwrap().data_plane.as_deref(),
        Some("remote_paged_ranges_v1")
    );
    assert!(rig.store.retention().unwrap().pins.is_empty());
    assert!(rig.store.read_object(&graph.directory).is_err());
    assert!(graph
        .source
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|reference| !graph.chunks.contains(reference)));
    let inode = rig.view.lookup(1, &graph.large_name).unwrap();
    assert_eq!(inode.size, 208 * 1024 * 1024);
    assert_ne!(inode.size, graph.file.size_bytes);
    assert_eq!(rig.view.read(inode.ino, 11, 7).unwrap(), b"qqqqqqq");
    assert_eq!(
        rig.view.read(inode.ino, CHUNK_BYTES as u64 + 5, 7).unwrap(),
        b"rrrrrrr"
    );
    let chunks = graph
        .source
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|r| graph.chunks.contains(r))
        .count();
    assert_eq!(chunks, 2);
    assert!(rig.cache.stats().unwrap().disk_bytes < 9 * 1024 * 1024);
    assert_eq!(private_bytes(&rig.manager, &rig.view), 0);
    // Opening remains cheap; remote readdir returns a bounded name/type page.
    let handle = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    assert_eq!(
        vfs::errno(&rig.view.directory_handle(1, handle).unwrap_err()),
        libc::EOPNOTSUPP
    );
    let page = rig.view.read_directory_page(1, handle, 0, 256).unwrap();
    assert_eq!(page.entries.len(), 256);
    assert!(!page.eof);
    rig.view.close_handle(1, handle).unwrap();
    let before = rig.store.retention().unwrap();
    assert_eq!(
        vfs::errno(&rig.view.checkpoint_snapshot_unmounted().unwrap_err()),
        libc::EOPNOTSUPP
    );
    assert_eq!(vfs::errno(&rig.view.seal().unwrap_err()), libc::EOPNOTSUPP);
    assert_eq!(
        vfs::errno(
            &rig.view
                .observe_source(
                    &ImportPolicy::default(),
                    &Uuid::new_v4().to_string(),
                    &ObservationBudget::default()
                )
                .unwrap_err()
        ),
        libc::EOPNOTSUPP
    );
    let after = rig.store.retention().unwrap();
    assert_eq!(before.pins, after.pins);
    assert_eq!(before.staging, after.staging);
    rig.view.write(inode.ino, 11, b"changed", true).unwrap();
    assert_eq!(private_bytes(&rig.manager, &rig.view), 65536);
    eprintln!(
        "{}",
        serde_json::json!({"event":"logical-remote-vfs","entries":100000,"fixture_ms":fixture_ms,"constructor_and_fork_ms":launch_ms,"logical_file_bytes":inode.size,"chunk_fetches":chunks,"partial_cache_bytes":rig.cache.stats().unwrap().disk_bytes,"local_complete_pins":after.pins.len(),"private_copyup_bytes":private_bytes(&rig.manager,&rig.view)})
    );
}

#[test]
fn sibling_range_cow_runtime_links_unlink_and_origin_rename_survive_reopen() {
    let _serial = serial();
    let graph = graph(2);
    let rig = rig(&graph, RemoteOptions::default());
    let sibling_session = session(&rig.registry, &graph);
    let sibling_manager = VfsManager::open_remote(
        rig.manager.root(),
        rig.store.clone(),
        sibling_session,
        &graph.root,
        RemoteOptions::default(),
    )
    .unwrap();
    let sibling = sibling_manager.fork_remote().unwrap();
    let inode = rig.view.lookup(1, &graph.large_name).unwrap();
    rig.view.write(inode.ino, 11, b"changed", false).unwrap();
    assert_eq!(private_bytes(&rig.manager, &rig.view), 65536);
    let sibling_inode = sibling.lookup(1, &graph.large_name).unwrap();
    assert_eq!(sibling.read(sibling_inode.ino, 11, 7).unwrap(), b"qqqqqqq");
    assert_eq!(rig.view.read(inode.ino, 11, 7).unwrap(), b"changed");
    let directory = rig.view.lookup(1, b"dir").unwrap();
    rig.view.rename(1, b"dir", 1, b"renamed", 0).unwrap();
    let child = rig.view.lookup(directory.ino, b"child").unwrap();
    assert_eq!(rig.view.read(child.ino, 0, 6).unwrap(), b"nested");
    let private = rig
        .view
        .create(1, b"private", NodeKind::Directory, 0o700, b"")
        .unwrap();
    rig.view
        .rename(directory.ino, b"child", private.ino, b"moved", 0)
        .unwrap();
    rig.view.link(child.ino, 1, b"linked").unwrap();
    assert_eq!(rig.view.lookup(1, b"linked").unwrap().ino, child.ino);
    rig.view.unlink(private.ino, b"moved", false).unwrap();
    assert_eq!(rig.view.attr(child.ino).unwrap().nlink, 1);
    let handle = rig
        .view
        .open_handle(child.ino, libc::O_RDWR, false)
        .unwrap();
    rig.view.unlink(1, b"linked", false).unwrap();
    assert_eq!(rig.view.attr(child.ino).unwrap().nlink, 0);
    assert_eq!(rig.view.read(child.ino, 0, 6).unwrap(), b"nested");
    rig.view.close_handle(child.ino, handle).unwrap();
    let Rig {
        _dir,
        store,
        cache,
        registry,
        authority,
        session,
        manager,
        view,
    } = rig;
    drop(view);
    let view = manager.open_remote_view().unwrap();
    let inode = view.lookup(1, &graph.large_name).unwrap();
    assert_eq!(view.read(inode.ino, 11, 7).unwrap(), b"changed");
    assert_eq!(view.lookup(1, b"renamed").unwrap().ino, directory.ino);
    assert!(view.lookup(1, b"dir").is_err());
    assert!(store.retention().unwrap().pins.is_empty());
    drop((
        view,
        sibling,
        sibling_manager,
        session,
        manager,
        registry,
        authority,
        cache,
    ));
}

#[test]
fn stale_authority_blocks_private_and_inherited_operations_without_refetch() {
    let _serial = serial();
    let graph = graph(1);
    let rig = rig(&graph, RemoteOptions::default());
    let file = rig.view.lookup(1, &graph.large_name).unwrap();
    rig.view.write(file.ino, 0, b"private", true).unwrap();
    let before = graph.source.calls.lock().unwrap().len();
    rig.authority.0.store(true, Ordering::Release);
    rig.session.invalidate();
    assert_eq!(
        vfs::errno(&rig.view.read(file.ino, 0, 7).unwrap_err()),
        libc::ESTALE
    );
    assert_eq!(
        vfs::errno(&rig.view.write(file.ino, 0, b"x", true).unwrap_err()),
        libc::ESTALE
    );
    assert_eq!(
        vfs::errno(&rig.view.lookup(1, b"dir").unwrap_err()),
        libc::ESTALE
    );
    assert_eq!(graph.source.calls.lock().unwrap().len(), before);
}

#[test]
fn unavailable_provider_never_falls_back_to_arbitrary_local_cas_presence() {
    let _serial = serial();
    let graph = graph(1);
    let rig = rig(&graph, RemoteOptions::default());
    let file = rig.view.lookup(1, &graph.large_name).unwrap();
    let bytes = DirectoryStore::read(graph.source.as_ref(), &graph.chunks[0]).unwrap();
    assert_eq!(rig.store.put_blob(&bytes).unwrap(), graph.chunks[0]);
    graph.source.fail.store(true, Ordering::Release);
    assert_eq!(
        vfs::errno(&rig.view.read(file.ino, 11, 7).unwrap_err()),
        libc::EIO
    );
    assert_eq!(private_bytes(&rig.manager, &rig.view), 0);
    assert!(rig.store.retention().unwrap().pins.is_empty());
}

#[test]
fn actual_file_length_mismatch_fails_before_creating_an_inherited_inode() {
    let _serial = serial();
    let mut graph = graph(1);
    let directory = PagedDirectory::new(graph.source.as_ref())
        .build(
            0o755,
            vec![DirectoryEntry::new(
                b"wrong",
                EntryNode::File {
                    object: graph.file.clone(),
                    logical_size: 208 * 1024 * 1024 + 1,
                    mode: 0o644,
                },
            )
            .unwrap()],
        )
        .unwrap();
    graph.directory = directory;
    graph.root = wrap(
        graph.source.as_ref(),
        &graph.directory,
        serde_json::json!({"format_version":1,"groups":[]}),
    );
    graph.source.publish_fixture(&graph.root);
    let rig = rig(&graph, RemoteOptions::default());
    assert_eq!(
        vfs::errno(&rig.view.lookup(1, b"wrong").unwrap_err()),
        libc::EIO
    );
    assert_eq!(rig.view.descriptor().unwrap().inode_count, 1);
}

#[test]
fn cached_proofs_are_bounded_and_local_mode_cannot_reopen_a_partial_view() {
    let _serial = serial();
    let graph = graph(1000);
    let rig = rig(
        &graph,
        RemoteOptions {
            max_cached_objects: 4,
            max_cached_bytes: CHUNK_BYTES as u64,
            ..Default::default()
        },
    );
    assert_eq!(
        vfs::errno(&rig.manager.prepare(&graph.directory).unwrap_err()),
        libc::EOPNOTSUPP
    );
    assert_eq!(
        vfs::errno(&rig.manager.prepare_paged(&graph.directory).unwrap_err()),
        libc::EOPNOTSUPP
    );
    assert!(rig.store.retention().unwrap().pins.is_empty());
    for index in 0..20 {
        rig.view
            .lookup(1, format!("file-{index:08}").as_bytes())
            .unwrap();
        let stats = rig.manager.remote_stats().unwrap().unwrap();
        assert!(stats.cached_objects <= 4);
        assert!(stats.cached_encoded_bytes <= CHUNK_BYTES as u64);
    }
    let local = VfsManager::open(rig.manager.root(), rig.store.clone()).unwrap();
    let id = rig.view.id().to_string();
    let owner = rig.session.attempt().owner.clone();
    let Rig {
        _dir,
        store,
        cache,
        registry,
        authority,
        session,
        manager,
        view,
    } = rig;
    drop(view);
    assert_eq!(
        vfs::errno(&local.open_view(&id, &owner).err().unwrap()),
        libc::EOPNOTSUPP
    );
    let view = manager.open_remote_view().unwrap();
    assert_eq!(view.id(), id);
    drop((
        view, manager, local, session, registry, authority, cache, store,
    ));
}

#[test]
fn local_nonblocking_directory_open_defers_enumeration_but_ordinary_open_stays_eager() {
    let _serial = serial();
    let dir = temp();
    let store = LocalStore::open(dir.path().join("cas")).unwrap();
    let blob = store.put_blob(b"local").unwrap();
    let root = store
        .put_directory(rudder_engine::store::DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: [b"a", b"b"]
                .into_iter()
                .map(|name| {
                    rudder_engine::store::DirectoryEntry::new(
                        name,
                        rudder_engine::store::EntryNode::File {
                            object: blob.clone(),
                            mode: 0o644,
                        },
                    )
                    .unwrap()
                })
                .collect(),
        })
        .unwrap();
    let manager = VfsManager::open(dir.path().join("manager"), store).unwrap();
    manager.prepare_paged(&root).unwrap();
    let view = manager
        .fork_paged(&Uuid::new_v4().to_string(), &root, attempt().owner)
        .unwrap();
    let handle = view
        .open_handle(1, libc::O_RDONLY | libc::O_NONBLOCK, true)
        .unwrap();
    assert_eq!(view.descriptor().unwrap().inode_count, 1);
    assert_eq!(view.directory_handle(1, handle).unwrap().len(), 4);
    assert_eq!(view.descriptor().unwrap().inode_count, 3);
    view.close_handle(1, handle).unwrap();
    let view = manager
        .fork_paged(&Uuid::new_v4().to_string(), &root, attempt().owner)
        .unwrap();
    view.open_handle(1, libc::O_RDONLY, true).unwrap();
    assert_eq!(view.descriptor().unwrap().inode_count, 3);
}

#[test]
fn inherited_hardlink_topology_requires_explicit_bulk_and_bad_expected_root_is_rejected() {
    let _serial = serial();
    let mut graph = graph(2);
    graph.root = wrap(
        graph.source.as_ref(),
        &graph.directory,
        serde_json::json!({"format_version":1,"groups":[{"paths_hex":[[hex::encode(b"file-00000000")],[hex::encode(b"file-00000001")]]}]}),
    );
    graph.source.publish_fixture(&graph.root);
    let dir = temp();
    for name in ["cache", "registry"] {
        fs::create_dir(dir.path().join(name)).unwrap();
        fs::set_permissions(dir.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let store = LocalStore::open(dir.path().join("cas")).unwrap();
    let cache = RemoteCache::open(
        dir.path().join("cache"),
        graph.source.clone(),
        CacheLimits::default(),
    )
    .unwrap();
    let registry = RemoteRootRegistry::open(
        dir.path().join("registry"),
        cache,
        graph.source.clone(),
        Arc::new(Authority(AtomicBool::new(false))),
        RootLimits::default(),
    )
    .unwrap();
    let session = session(&registry, &graph);
    let error = VfsManager::open_remote(
        dir.path().join("manager"),
        store.clone(),
        session.clone(),
        &graph.root,
        RemoteOptions::default(),
    )
    .err()
    .unwrap();
    assert_eq!(vfs::errno(&error), libc::EOPNOTSUPP);
    assert!(!dir.path().join("manager").exists());
    assert!(store.retention().unwrap().pins.is_empty());
    let mut different = graph.root.clone();
    different.digest = "a".repeat(64);
    assert!(VfsManager::open_remote(
        dir.path().join("wrong"),
        store,
        session,
        &different,
        RemoteOptions::default()
    )
    .is_err());
}

fn directory_rows(
    view: &VfsView,
    ino: u64,
    handle: u64,
    size: usize,
) -> Vec<vfs::DirectoryListingEntry> {
    let mut cookie = 0;
    let mut result = Vec::new();
    loop {
        let page = view.read_directory_page(ino, handle, cookie, size).unwrap();
        assert!(page.entries.len() <= size);
        if let Some(last) = page.entries.last() {
            assert_ne!(last.next_cookie, cookie);
            cookie = last.next_cookie;
        } else {
            assert!(page.eof, "empty non-final directory page");
        }
        result.extend(page.entries);
        if page.eof {
            break;
        }
    }
    result
}

#[test]
fn remote_readdir_over_100k_entries_uses_bounded_pages_without_file_or_blob_hydration() {
    let _serial = serial();
    let graph = graph(100_100);
    let rig = rig(&graph, RemoteOptions::default());
    let before = graph.source.calls.lock().unwrap().len();
    let handle = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    assert_eq!(rig.view.readdir_stats().unwrap().unwrap().live_cursors, 0);
    let started = Instant::now();
    let rows = directory_rows(&rig.view, 1, handle, 256);
    let listing_ms = started.elapsed().as_millis();
    let listing_stats = rig.view.readdir_stats().unwrap().unwrap();
    assert_eq!(listing_stats.namespace_identity_commits, 392);
    assert_eq!(listing_stats.cursor_commits, 392);
    assert_eq!(rows.len(), 100_103); // files + inherited directory + dot entries
    assert!(rows[2..].windows(2).all(|r| r[0].name < r[1].name));
    assert!(rows[2..].iter().all(|r| r.ino > 1));
    assert_eq!(
        rows[2..]
            .iter()
            .map(|r| r.ino)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        100_101
    );
    let calls = graph.source.calls.lock().unwrap();
    assert!(calls[before..]
        .iter()
        .all(|r| r.kind == ObjectKind::DirectoryShard));
    let metadata_requests = calls.len() - before;
    drop(calls);
    assert!(!graph.source.calls.lock().unwrap().contains(&graph.file));
    assert!(graph
        .chunks
        .iter()
        .all(|r| !graph.source.calls.lock().unwrap().contains(r)));
    assert_eq!(
        rig.view.descriptor().unwrap().inode_count,
        1,
        "listing must not synthesize attributes"
    );
    assert_eq!(rig.view.descriptor().unwrap().format_version, 3);
    let stats = rig.view.readdir_stats().unwrap().unwrap();
    assert_eq!(stats.live_cursors, 1);
    assert_eq!(stats.cookies, 100_101);
    assert_eq!(stats.reserved_inodes, 100_101);
    assert!(stats.cursor_db_bytes <= 128 * 1024 * 1024);
    let wal = rig
        .manager
        .root()
        .join("views")
        .join(rig.view.id())
        .join("namespace.sqlite-wal");
    assert_eq!(fs::metadata(wal).map(|m| m.len()).unwrap_or(0), 0);
    let first = &rows[3];
    let actual = rig.view.lookup(1, &first.name).unwrap();
    assert_eq!(actual.ino, first.ino);
    assert_eq!(actual.size, 208 * 1024 * 1024);
    let private = rig
        .view
        .create(1, b"private", NodeKind::File, 0o600, b"")
        .unwrap();
    assert!(private.ino > rows.iter().map(|r| r.ino).max().unwrap());
    // A reply buffer may stop halfway through a precomputed page: every issued
    // cookie remains valid and does not allocate a second identity/cookie.
    let resumed = rig
        .view
        .read_directory_page(1, handle, rows[49].next_cookie, 256)
        .unwrap();
    assert_eq!(resumed.entries, rows[50..306]);
    let rewound = rig.view.read_directory_page(1, handle, 0, 256).unwrap();
    assert_eq!(rewound.entries, rows[..256]);
    assert_eq!(rig.view.readdir_stats().unwrap().unwrap().cookies, 100_101);
    rig.view.close_handle(1, handle).unwrap();
    let stats = rig.view.readdir_stats().unwrap().unwrap();
    assert_eq!(stats.live_cursors, 0);
    assert_eq!(stats.cookies, 0);
    assert_eq!(stats.overlay_rows, 0);
    eprintln!(
        "{}",
        serde_json::json!({"phase":"remote_readdir_100k","entries":100_101,"elapsed_ms":started.elapsed().as_millis(),"listing_ms":listing_ms,"listing_stages":listing_stats,"metadata_requests":metadata_requests,"file_or_blob_requests":0,"max_page_entries":256,"cursor_db_bytes":stats.cursor_db_bytes})
    );
}

#[test]
fn remote_directory_cookies_freeze_names_and_identity_across_namespace_mutations() {
    let _serial = serial();
    let graph = graph(28);
    let rig = rig(&graph, RemoteOptions::default());
    for name in [b"a".as_slice(), b"aa", b"private", b"\xff"] {
        rig.view
            .create(1, name, NodeKind::File, 0o600, b"")
            .unwrap();
    }
    let handle = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    let frozen = directory_rows(&rig.view, 1, handle, 7);
    let old = frozen.iter().find(|r| r.name == b"file-00000000").unwrap();
    rig.view
        .rename(1, b"file-00000000", 1, b"zz-renamed", 0)
        .unwrap();
    rig.view.unlink(1, b"file-00000001", false).unwrap();
    let linked = rig.view.lookup(1, b"file-00000002").unwrap();
    rig.view.link(linked.ino, 1, b"zz-hardlink").unwrap();
    rig.view
        .create(1, b"new", NodeKind::Directory, 0o700, b"")
        .unwrap();
    rig.view.unlink(1, b"private", false).unwrap();
    assert_eq!(directory_rows(&rig.view, 1, handle, 1), frozen);
    assert_eq!(rig.view.lookup(1, b"zz-renamed").unwrap().ino, old.ino);
    assert_eq!(
        vfs::errno(
            &rig.view
                .read_directory_page(1, handle, u64::MAX, 7)
                .unwrap_err()
        ),
        libc::EINVAL
    );
    assert_eq!(
        vfs::errno(&rig.view.read_directory_page(1, handle, 0, 0).unwrap_err()),
        libc::EINVAL
    );
    let next = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    // A cookie issued to another open handle has not been issued here.
    assert_eq!(
        vfs::errno(
            &rig.view
                .read_directory_page(1, next, old.next_cookie, 7)
                .unwrap_err()
        ),
        libc::EINVAL
    );
    let current = directory_rows(&rig.view, 1, next, 11);
    assert!(!current
        .iter()
        .any(|r| r.name == b"file-00000000" || r.name == b"file-00000001" || r.name == b"private"));
    assert!(current.iter().any(|r| r.name == b"new"));
    assert!(current[2..].windows(2).all(|r| r[0].name < r[1].name));
    assert_eq!(
        current
            .iter()
            .find(|r| r.name == b"zz-renamed")
            .unwrap()
            .ino,
        old.ino
    );
    assert_eq!(
        current
            .iter()
            .find(|r| r.name == b"zz-hardlink")
            .unwrap()
            .ino,
        linked.ino
    );
    rig.view.close_handle(1, handle).unwrap();
    assert_eq!(
        vfs::errno(&rig.view.read_directory_page(1, handle, 0, 7).unwrap_err()),
        libc::EBADF
    );
    rig.view.close_handle(1, next).unwrap();
    let stats = rig.view.readdir_stats().unwrap().unwrap();
    assert_eq!(
        (stats.live_cursors, stats.cookies, stats.overlay_rows),
        (0, 0, 0)
    );
}

#[test]
fn remote_readdir_reservations_never_return_unverified_child_attributes() {
    let _serial = serial();
    let graph = graph(4);
    // Replace the parent entry's logical length while preserving the real File
    // descriptor. Directory-local schema remains valid; the actual child edge
    // must fail when attributes are requested, not be trusted as a stat hint.
    let entries = vec![DirectoryEntry::new(
        b"bad",
        EntryNode::File {
            object: graph.file.clone(),
            logical_size: 1,
            mode: 0o600,
        },
    )
    .unwrap()];
    let directory = PagedDirectory::new(graph.source.as_ref())
        .build(0o755, entries)
        .unwrap();
    let root = wrap(
        &graph.source,
        &directory,
        serde_json::json!({"format_version":1,"groups":[]}),
    );
    graph.source.publish_fixture(&root);
    let graph = Graph {
        root,
        directory,
        ..graph
    };
    let rig = rig(&graph, RemoteOptions::default());
    let handle = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    let rows = directory_rows(&rig.view, 1, handle, 1);
    let listed = rows.iter().find(|e| e.name == b"bad").unwrap();
    assert_eq!(listed.kind, NodeKind::File);
    assert!(!graph.source.calls.lock().unwrap().contains(&graph.file));
    assert!(rig.view.attr(listed.ino).is_err());
    assert_eq!(rig.view.descriptor().unwrap().inode_count, 1);
    rig.view.close_handle(1, handle).unwrap();
}

#[test]
fn remote_readdir_cookie_and_live_cursor_quotas_leave_existing_seeks_usable() {
    let _serial = serial();
    let graph = graph(16);
    let mut options = RemoteOptions::default();
    options.readdir.max_live_cursors = 1;
    options.readdir.max_cookies_per_cursor = 4;
    options.readdir.max_total_cookies = 4;
    let rig = rig(&graph, options);
    let first = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    let second = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    let page = rig.view.read_directory_page(1, first, 0, 6).unwrap();
    assert_eq!(page.entries.len(), 6);
    assert_eq!(
        vfs::errno(&rig.view.read_directory_page(1, second, 0, 2).unwrap_err()),
        libc::EAGAIN
    );
    assert_eq!(
        vfs::errno(
            &rig.view
                .read_directory_page(1, first, page.entries.last().unwrap().next_cookie, 1)
                .unwrap_err()
        ),
        libc::EAGAIN
    );
    assert_eq!(
        rig.view
            .read_directory_page(1, first, 0, 6)
            .unwrap()
            .entries,
        page.entries
    );
    assert_eq!(rig.view.readdir_stats().unwrap().unwrap().cookies, 4);
    rig.view.close_handle(1, first).unwrap();
    assert_eq!(
        rig.view
            .read_directory_page(1, second, 0, 3)
            .unwrap()
            .entries
            .len(),
        3
    );
    rig.view.close_handle(1, second).unwrap();
    assert_eq!(rig.view.readdir_stats().unwrap().unwrap().cookies, 0);
}

#[test]
fn remote_readdir_overlay_and_identity_quotas_are_checked_before_cursor_publication() {
    let _serial = serial();
    let graph = graph(10);
    let mut options = RemoteOptions::default();
    options.readdir.max_overlay_rows_per_cursor = 1;
    options.readdir.max_reserved_inodes = 2;
    let rig = rig(&graph, options);
    let first = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    let page = rig.view.read_directory_page(1, first, 0, 4).unwrap();
    assert_eq!(page.entries.len(), 4);
    assert_eq!(
        vfs::errno(
            &rig.view
                .read_directory_page(1, first, page.entries.last().unwrap().next_cookie, 1)
                .unwrap_err()
        ),
        libc::EAGAIN
    );
    rig.view
        .create(1, b"private-a", NodeKind::File, 0o600, b"")
        .unwrap();
    rig.view
        .create(1, b"private-b", NodeKind::File, 0o600, b"")
        .unwrap();
    let next = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    assert_eq!(
        vfs::errno(&rig.view.read_directory_page(1, next, 0, 2).unwrap_err()),
        libc::EAGAIN
    );
    assert_eq!(rig.view.readdir_stats().unwrap().unwrap().live_cursors, 1);
    assert_eq!(
        rig.view
            .read_directory_page(1, first, 0, 4)
            .unwrap()
            .entries,
        page.entries
    );
    rig.view.close_handle(1, next).unwrap();
    rig.view.close_handle(1, first).unwrap();
}

#[test]
fn remote_readdir_physical_cursor_disk_quota_rolls_back_without_losing_prior_cursor() {
    let _serial = serial();
    let graph = graph(2000);
    let mut options = RemoteOptions::default();
    options.readdir.max_cursor_db_bytes = 64 * 1024;
    let rig = rig(&graph, options);
    let handle = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    let dots = rig.view.read_directory_page(1, handle, 0, 2).unwrap();
    let mut cookie = 2;
    let mut quota = false;
    for _ in 0..20 {
        match rig.view.read_directory_page(1, handle, cookie, 256) {
            Ok(page) => cookie = page.entries.last().unwrap().next_cookie,
            Err(e) => {
                assert_eq!(vfs::errno(&e), libc::EAGAIN, "{e:#}");
                quota = true;
                break;
            }
        }
    }
    assert!(quota);
    assert_eq!(
        rig.view
            .read_directory_page(1, handle, 0, 2)
            .unwrap()
            .entries,
        dots.entries
    );
    assert!(rig.view.readdir_stats().unwrap().unwrap().cursor_db_bytes <= 64 * 1024);
    let path = rig
        .manager
        .root()
        .join("views")
        .join(rig.view.id())
        .join("readdir.sqlite-journal");
    assert!(!path.exists());
    rig.view.close_handle(1, handle).unwrap();
    assert_eq!(rig.view.readdir_stats().unwrap().unwrap().cookies, 0);
}

#[test]
fn remote_readdir_reopen_discards_cursors_but_preserves_reserved_identity() {
    let _serial = serial();
    let graph = graph(24);
    let rig = rig(&graph, RemoteOptions::default());
    let handle = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    let rows = directory_rows(&rig.view, 1, handle, 17);
    let file = rows
        .iter()
        .find(|r| r.name == b"file-00000000")
        .unwrap()
        .clone();
    assert_eq!(rig.view.descriptor().unwrap().inode_count, 1);
    let Rig {
        view,
        manager,
        _dir,
        ..
    } = rig;
    drop(view); // model process ownership loss; do not close cursor explicitly
    let view = manager.open_remote_view().unwrap();
    assert_eq!(view.readdir_stats().unwrap().unwrap().live_cursors, 0);
    assert_eq!(view.readdir_stats().unwrap().unwrap().cookies, 0);
    assert_eq!(view.attr(file.ino).unwrap().size, 208 * 1024 * 1024);
    assert_eq!(view.lookup(1, &file.name).unwrap().ino, file.ino);
    let next = view.open_handle(1, libc::O_RDONLY, true).unwrap();
    assert_eq!(
        vfs::errno(
            &view
                .read_directory_page(1, next, file.next_cookie, 4)
                .unwrap_err()
        ),
        libc::EINVAL
    );
    assert_eq!(directory_rows(&view, 1, next, 17), rows);
    view.close_handle(1, next).unwrap();
    drop(_dir);
}

#[test]
fn remote_readdir_raw_prefixes_directory_moves_and_revocation_preserve_cursor_contract() {
    let _serial = serial();
    let mut graph = graph(600);
    let mut entries = Vec::new();
    let empty = PagedDirectory::new(graph.source.as_ref())
        .build(0o700, [])
        .unwrap();
    for name in [
        b"a".as_slice(),
        b"aa",
        b"a\x01",
        b"\xff",
        b"\xffa",
        b"directory-a",
        b"directory-aa",
    ] {
        entries.push(
            DirectoryEntry::new(
                name,
                if name.starts_with(b"directory") {
                    EntryNode::Directory {
                        object: empty.clone(),
                    }
                } else {
                    EntryNode::File {
                        object: graph.file.clone(),
                        logical_size: 208 * 1024 * 1024,
                        mode: 0o600,
                    }
                },
            )
            .unwrap(),
        );
    }
    // Force multiple radix pages alongside prefix/terminator names.
    for index in 0..600 {
        entries.push(
            DirectoryEntry::new(
                format!("a-{index:08}-{}", "z".repeat(120)).as_bytes(),
                EntryNode::File {
                    object: graph.file.clone(),
                    logical_size: 208 * 1024 * 1024,
                    mode: 0o600,
                },
            )
            .unwrap(),
        );
    }
    entries.sort_by_key(|e| e.name().unwrap());
    graph.directory = PagedDirectory::new(graph.source.as_ref())
        .build(0o755, entries)
        .unwrap();
    graph.root = wrap(
        &graph.source,
        &graph.directory,
        serde_json::json!({"format_version":1,"groups":[]}),
    );
    graph.source.publish_fixture(&graph.root);
    let rig = rig(&graph, RemoteOptions::default());
    let handle = rig.view.open_handle(1, libc::O_RDONLY, true).unwrap();
    let rows = directory_rows(&rig.view, 1, handle, 13);
    assert_eq!(rows.len(), 609);
    assert!(rows[2..].windows(2).all(|e| e[0].name < e[1].name));
    assert_eq!(rig.view.descriptor().unwrap().inode_count, 1);
    let left = rows.iter().find(|e| e.name == b"directory-a").unwrap();
    let right = rows.iter().find(|e| e.name == b"directory-aa").unwrap();
    assert_ne!(
        left.ino, right.ino,
        "equal empty directories are separate inodes"
    );
    assert_eq!(rig.view.attr(left.ino).unwrap().ino, left.ino);
    let own = rig
        .view
        .create(1, b"new-parent", NodeKind::Directory, 0o700, b"")
        .unwrap();
    let child = rig
        .view
        .open_handle(left.ino, libc::O_RDONLY, true)
        .unwrap();
    let old = directory_rows(&rig.view, left.ino, child, 1);
    rig.view
        .rename(1, b"directory-a", own.ino, b"moved", 0)
        .unwrap();
    assert_eq!(directory_rows(&rig.view, left.ino, child, 1), old);
    assert_eq!(old[1].ino, 1);
    let new = rig
        .view
        .open_handle(left.ino, libc::O_RDONLY, true)
        .unwrap();
    assert_eq!(directory_rows(&rig.view, left.ino, new, 1)[1].ino, own.ino);
    let calls = graph.source.calls.lock().unwrap().len();
    rig.authority.0.store(true, Ordering::Release);
    assert_eq!(
        vfs::errno(
            &rig.view
                .read_directory_page(1, handle, rows[10].next_cookie, 3)
                .unwrap_err()
        ),
        libc::ESTALE
    );
    assert_eq!(graph.source.calls.lock().unwrap().len(), calls);
    // Close is authority-independent cleanup of reconstructible cursors only.
    rig.view.close_handle(1, handle).unwrap();
    rig.view.close_handle(left.ino, child).unwrap();
    rig.view.close_handle(left.ino, new).unwrap();
    let cursor = rusqlite::Connection::open_with_flags(
        rig.manager
            .root()
            .join("views")
            .join(rig.view.id())
            .join("readdir.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        cursor
            .query_row("SELECT count(*) FROM cursors", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

fn crash_view(path: &Path, graph: &Graph, attempt: &AttemptBinding) -> (VfsManager, VfsView) {
    for name in ["cache", "registry"] {
        let p = path.join(name);
        if !p.exists() {
            fs::create_dir(&p).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let store = LocalStore::open(path.join("cas")).unwrap();
    let cache = RemoteCache::open(
        path.join("cache"),
        graph.source.clone(),
        CacheLimits::default(),
    )
    .unwrap();
    let registry = RemoteRootRegistry::open(
        path.join("registry"),
        cache,
        graph.source.clone(),
        Arc::new(Authority(AtomicBool::new(false))),
        RootLimits::default(),
    )
    .unwrap();
    let session = registry
        .admit(
            attempt,
            "prepared",
            &graph.root,
            &SourceRequestControl::new(Instant::now() + Duration::from_secs(5)),
        )
        .unwrap();
    let manager = VfsManager::open_remote(
        path.join("manager"),
        store,
        session,
        &graph.root,
        RemoteOptions::default(),
    )
    .unwrap();
    let view = manager.fork_remote().unwrap();
    (manager, view)
}
#[test]
fn remote_readdir_crash_child() {
    let Some(path) = std::env::var_os("RUDDER_TEST_READDIR_CRASH_ROOT") else {
        return;
    };
    let path = PathBuf::from(path);
    let attempt: AttemptBinding =
        serde_json::from_slice(&fs::read(path.join("attempt.json")).unwrap()).unwrap();
    let mut graph = graph(32);
    Arc::get_mut(&mut graph.source).unwrap().scope.tenant_id = attempt.view_id.clone();
    let (_manager, view) = crash_view(&path, &graph, &attempt);
    let handle = view.open_handle(1, libc::O_RDONLY, true).unwrap();
    let rows = directory_rows(&view, 1, handle, 9);
    let witness = rows
        .iter()
        .map(|r| (hex::encode(&r.name), r.ino, r.next_cookie))
        .collect::<Vec<_>>();
    let mut file = fs::File::create(path.join("acked.json")).unwrap();
    file.write_all(&serde_json::to_vec(&witness).unwrap())
        .unwrap();
    file.sync_all().unwrap();
    // No Drop, destructor flush, cache settlement or graceful SQLite close.
    unsafe { libc::_exit(86) }
}
#[test]
fn remote_readdir_acknowledged_reservations_survive_real_process_death() {
    let _serial = serial();
    let dir = temp();
    let attempt = attempt();
    fs::write(
        dir.path().join("attempt.json"),
        serde_json::to_vec(&attempt).unwrap(),
    )
    .unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "remote_readdir_crash_child", "--nocapture"])
        .env("RUDDER_TEST_READDIR_CRASH_ROOT", dir.path())
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(86),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let witness: Vec<(String, u64, u64)> =
        serde_json::from_slice(&fs::read(dir.path().join("acked.json")).unwrap()).unwrap();
    let mut graph = graph(32);
    Arc::get_mut(&mut graph.source).unwrap().scope.tenant_id = attempt.view_id.clone();
    let (manager, view) = crash_view(dir.path(), &graph, &attempt);
    assert_eq!(
        graph.source.root_calls.load(Ordering::Acquire),
        1,
        "restart must fetch fresh receipt authority"
    );
    assert_eq!(view.readdir_stats().unwrap().unwrap().live_cursors, 0);
    let handle = view.open_handle(1, libc::O_RDONLY, true).unwrap();
    let rows = directory_rows(&view, 1, handle, 9);
    let recovered = rows
        .iter()
        .map(|r| (hex::encode(&r.name), r.ino, r.next_cookie))
        .collect::<Vec<_>>();
    assert_eq!(recovered, witness);
    let file = rows.iter().find(|e| e.name == b"file-00000000").unwrap();
    assert_eq!(view.attr(file.ino).unwrap().size, 208 * 1024 * 1024);
    let fresh = view
        .create(1, b"after-crash", NodeKind::File, 0o600, b"")
        .unwrap();
    assert!(fresh.ino > witness.iter().map(|r| r.1).max().unwrap());
    assert!(manager.store().retention().unwrap().pins.is_empty());
    view.close_handle(1, handle).unwrap();
}
