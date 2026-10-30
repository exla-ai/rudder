use anyhow::{bail, Context, Result};
use rudder_engine::{
    content::{FileNode, FILE_LAYOUT},
    directory::{self, DirectoryEntry, DirectoryStore, EntryNode, PagedDirectory, ShardNode},
    protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef, OwnerEpoch},
    remote_cache::{CacheLimits, CacheScope, FetchControl, ImmutableObjectSource, RemoteCache},
    remote_root::{
        AttemptBinding, CurrentAttemptAuthority, LoadedObject, ObjectHandle, RemoteRootRegistry,
        RemoteSession, RootBinding, RootError, RootLimits, RootReceiptSource,
    },
    remote_source::{
        NodeRemoteSource, NodeSourceConfig, ReplicatedDurability, ReplicatedRootReceipt,
        SourceError, SourceRequestControl, TransportProfile,
    },
    snapshot::{self, RunnableRoot, SourceV2},
};
use std::{
    collections::BTreeMap,
    fs,
    future::Future,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    task::{Context as TaskContext, Wake, Waker},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use uuid::Uuid;

mod partial_tree_deltas {
    use super::*;
    use rudder_engine::{
        delta::{self, DeltaBudget, PartialTreeReceipt},
        store::LocalStore,
    };

    fn topology(session: &RemoteSession) -> ObjectHandle {
        let root = load(session, &session.root().unwrap());
        let manifest = root.with_bytes(snapshot::decode_runnable).unwrap().unwrap();
        root.child(&manifest.hardlinks).unwrap()
    }
    fn inherited_file(graph: &Graph, session: &RemoteSession) -> ObjectHandle {
        let (leaf, _) = lookup(session, view(session), b"file-00000000").unwrap();
        leaf.child(&graph.file).unwrap()
    }
    fn changed_tree(
        store: &LocalStore,
        stage: &str,
        inherited: &ObjectRef,
        logical_size: u64,
    ) -> (ObjectRef, Vec<ObjectRef>) {
        let changed = store.put_blob(b"new local content").unwrap();
        let directory = PagedDirectory::new(&directory::LocalDirectoryStore::new(store, stage))
            .build(
                0o755,
                vec![
                    DirectoryEntry::new(
                        b"changed",
                        EntryNode::File {
                            object: changed.clone(),
                            logical_size: changed.size_bytes,
                            mode: 0o644,
                        },
                    )
                    .unwrap(),
                    DirectoryEntry::new(
                        b"inherited",
                        EntryNode::File {
                            object: inherited.clone(),
                            logical_size,
                            mode: 0o644,
                        },
                    )
                    .unwrap(),
                ],
            )
            .unwrap();
        let shard = directory::decode_directory_root(&store.read_object(&directory).unwrap())
            .unwrap()
            .directory_shard;
        (directory.clone(), vec![changed, shard, directory])
    }

    #[test]
    fn partial_plan_and_new_bytes_are_retained_atomically_without_hydrating_old_data() {
        let graph = graph(1, false);
        let setup = build_setup(&graph, RootLimits::default());
        let temp = temp();
        let store = LocalStore::open(temp.path().join("new-cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let (directory, mut objects) =
            changed_tree(&store, &stage.id, &graph.file, graph.file.size_bytes);
        let orphan = store.put_blob(b"intermediate producer output").unwrap();
        objects.push(orphan.clone());
        let links = topology(&setup.session);
        let boundaries = vec![inherited_file(&graph, &setup.session), links.clone()];
        let capture = Uuid::new_v4().to_string();
        let retained = delta::retain_partial_tree(
            &store,
            &stage.id,
            "partial-test",
            &capture,
            &setup.session,
            &directory,
            links.reference(),
            &objects,
            &boundaries,
            &DeltaBudget::default(),
        )
        .unwrap();
        assert_eq!(retained.plan().base, *setup.session.binding());
        assert_eq!(retained.plan().attempt, setup.attempt);
        assert!(store.retention().unwrap().pins.is_empty());
        assert!(!retained.plan().local_objects.contains(&orphan));
        assert!(store.retained_closure(&directory).is_err());
        store.gc(Duration::ZERO).unwrap();
        assert!(store.read_object(&orphan).is_err());
        assert!(store.read_object(&graph.file).is_err());
        assert!(
            !graph.source.calls.lock().unwrap().contains(&graph.file),
            "metadata capture hydrated inherited file bytes"
        );
        assert_eq!(
            delta::read_partial_tree(&store, retained.receipt()).unwrap(),
            *retained.plan()
        );
        let retry = delta::retain_partial_tree(
            &store,
            &stage.id,
            "partial-test",
            &capture,
            &setup.session,
            &directory,
            links.reference(),
            &objects,
            &boundaries,
            &DeltaBudget::default(),
        )
        .unwrap();
        assert_eq!(retry.receipt(), retained.receipt());
        assert!(
            delta::retain_partial_tree(
                &store,
                &stage.id,
                "partial-test",
                &Uuid::new_v4().to_string(),
                &setup.session,
                &directory,
                links.reference(),
                &objects,
                &boundaries,
                &DeltaBudget::default()
            )
            .is_err(),
            "same object-set pin cannot rebind another capture"
        );
    }

    #[test]
    fn all_remote_plan_still_binds_capture_identity_in_its_local_object_set() {
        let graph = graph(1, false);
        let setup = build_setup(&graph, RootLimits::default());
        let root = load(&setup.session, &setup.session.root().unwrap());
        let manifest = root.with_bytes(snapshot::decode_runnable).unwrap().unwrap();
        let directory = root.child(&manifest.view_root).unwrap();
        let links = root.child(&manifest.hardlinks).unwrap();
        let temp = temp();
        let store = LocalStore::open(temp.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let retained = delta::retain_partial_tree(
            &store,
            &stage.id,
            "all-remote",
            &Uuid::new_v4().to_string(),
            &setup.session,
            directory.reference(),
            links.reference(),
            &[],
            &[directory.clone(), links.clone()],
            &DeltaBudget::default(),
        )
        .unwrap();
        assert!(retained.plan().local_objects.is_empty());
        assert_eq!(
            retained.receipt().local.object_count,
            1,
            "plan Blob itself is durable"
        );
        assert!(delta::retain_partial_tree(
            &store,
            &stage.id,
            "all-remote",
            &Uuid::new_v4().to_string(),
            &setup.session,
            directory.reference(),
            links.reference(),
            &[],
            &[directory.clone(), links.clone()],
            &DeltaBudget::default()
        )
        .is_err());
    }

    #[test]
    fn transfer_retention_outlives_the_producer_without_granting_complete_local_closure() {
        use rudder_engine::{paths::FabricPaths, service::TransferSession};
        let graph = graph(1, false);
        let setup = build_setup(&graph, RootLimits::default());
        let temp = temp();
        let cas = temp.path().join("cas");
        let store = LocalStore::open(&cas).unwrap();
        let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let (directory, objects) =
            changed_tree(&store, &stage.id, &graph.file, graph.file.size_bytes);
        let links = topology(&setup.session);
        let retained = delta::retain_partial_tree(
            &store,
            &stage.id,
            "producer",
            &Uuid::new_v4().to_string(),
            &setup.session,
            &directory,
            links.reference(),
            &objects,
            &[inherited_file(&graph, &setup.session), links.clone()],
            &DeltaBudget::default(),
        )
        .unwrap();
        let paths = FabricPaths {
            root: temp.path().to_path_buf(),
            cas,
            journal: temp.path().join("unused.sqlite"),
        };
        let mut transfer = TransferSession::open(&paths).unwrap();
        let inventory = transfer.pin_partial_source(retained.receipt()).unwrap();
        assert!(inventory.owned_pin.starts_with("transfer:partial:rpc:"));
        assert_eq!(inventory.base, *setup.session.binding());
        assert_eq!(inventory.directory, directory);
        assert!(inventory.objects.contains(&retained.receipt().plan));
        assert!(transfer.inventory(&directory).is_err());
        assert!(transfer.read(&graph.file).is_err());
        assert!(transfer.unpin_source("producer").is_err());
        store.release_object_set(&retained.receipt().local).unwrap();
        store.gc(Duration::ZERO).unwrap();
        for object in &inventory.objects {
            assert_eq!(
                transfer.read(object).unwrap(),
                store.read_object(object).unwrap()
            );
        }
        assert!(store.retention().unwrap().pins.is_empty());
        assert!(store.retained_closure(&directory).is_err());
        assert!(!graph.source.calls.lock().unwrap().contains(&graph.file));
        transfer.finish().unwrap();
        store.gc(Duration::ZERO).unwrap();
        assert!(store.read_object(&retained.receipt().plan).is_err());
    }

    #[test]
    fn dropping_partial_transfer_preserves_the_original_workspace_retention() {
        use rudder_engine::{paths::FabricPaths, service::TransferSession};
        let graph = graph(1, false);
        let setup = build_setup(&graph, RootLimits::default());
        let root = load(&setup.session, &setup.session.root().unwrap());
        let manifest = root.with_bytes(snapshot::decode_runnable).unwrap().unwrap();
        let directory = root.child(&manifest.view_root).unwrap();
        let links = root.child(&manifest.hardlinks).unwrap();
        let temp = temp();
        let cas = temp.path().join("cas");
        let store = LocalStore::open(&cas).unwrap();
        let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let retained = delta::retain_partial_tree(
            &store,
            &stage.id,
            "producer",
            &Uuid::new_v4().to_string(),
            &setup.session,
            directory.reference(),
            links.reference(),
            &[],
            &[directory.clone(), links.clone()],
            &DeltaBudget::default(),
        )
        .unwrap();
        let paths = FabricPaths {
            root: temp.path().to_path_buf(),
            cas,
            journal: temp.path().join("unused.sqlite"),
        };
        {
            let mut transfer = TransferSession::open(&paths).unwrap();
            let inventory = transfer.pin_partial_source(retained.receipt()).unwrap();
            assert_eq!(inventory.objects, vec![retained.receipt().plan.clone()]);
        }
        store.gc(Duration::ZERO).unwrap();
        assert_eq!(
            delta::read_partial_tree(&store, retained.receipt()).unwrap(),
            *retained.plan()
        );
        assert!(store.retention().unwrap().staging.is_empty());
    }

    #[test]
    fn physical_local_presence_never_substitutes_for_producer_or_remote_provenance() {
        let graph = graph(1, false);
        let setup = build_setup(&graph, RootLimits::default());
        let temp = temp();
        let store = LocalStore::open(temp.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let bytes = graph.source.objects.lock().unwrap()[&graph.file].clone();
        store
            .receive_staged(&stage.id, &graph.file, &bytes)
            .unwrap();
        let (directory, objects) =
            changed_tree(&store, &stage.id, &graph.file, graph.file.size_bytes);
        let links = topology(&setup.session);
        assert!(delta::retain_partial_tree(
            &store,
            &stage.id,
            "no-fallback",
            &Uuid::new_v4().to_string(),
            &setup.session,
            &directory,
            links.reference(),
            &objects,
            &[links.clone()],
            &DeltaBudget::default()
        )
        .is_err());
        assert!(store.object_set_receipt("no-fallback").unwrap().is_none());
        assert!(store.retention().unwrap().staging.contains_key(&stage.id));
    }

    #[test]
    fn changed_parent_cannot_forge_inherited_file_length_or_cross_session_handles() {
        let graph = graph(1, true);
        let setup = build_setup(&graph, RootLimits::default());
        let foreign = build_setup(&graph, RootLimits::default());
        let temp = temp();
        let store = LocalStore::open(temp.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let (directory, objects) =
            changed_tree(&store, &stage.id, &graph.file, 208 * 1024 * 1024 + 1);
        let links = topology(&setup.session);
        assert!(delta::retain_partial_tree(
            &store,
            &stage.id,
            "forged",
            &Uuid::new_v4().to_string(),
            &setup.session,
            &directory,
            links.reference(),
            &objects,
            &[inherited_file(&graph, &setup.session), links.clone()],
            &DeltaBudget::default()
        )
        .is_err());
        assert!(delta::retain_partial_tree(
            &store,
            &stage.id,
            "foreign",
            &Uuid::new_v4().to_string(),
            &setup.session,
            &directory,
            links.reference(),
            &objects,
            &[inherited_file(&graph, &foreign.session), links.clone()],
            &DeltaBudget::default()
        )
        .is_err());
        assert!(store.object_set_receipt("forged").unwrap().is_none());
        assert!(graph.chunks.iter().all(|chunk| !graph
            .source
            .calls
            .lock()
            .unwrap()
            .contains(chunk)));
    }

    #[test]
    fn revoked_attempt_and_exhausted_budget_cannot_publish_partial_retention() {
        let graph = graph(1, false);
        let setup = build_setup(&graph, RootLimits::default());
        let temp = temp();
        let store = LocalStore::open(temp.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let (directory, objects) =
            changed_tree(&store, &stage.id, &graph.file, graph.file.size_bytes);
        let links = topology(&setup.session);
        let boundaries = vec![inherited_file(&graph, &setup.session), links.clone()];
        let mut budget = DeltaBudget::default();
        budget.max_metadata_bytes = 1;
        assert!(delta::retain_partial_tree(
            &store,
            &stage.id,
            "budget",
            &Uuid::new_v4().to_string(),
            &setup.session,
            &directory,
            links.reference(),
            &objects,
            &boundaries,
            &budget
        )
        .is_err());
        setup.authority.revoked.store(true, Ordering::Release);
        assert!(delta::retain_partial_tree(
            &store,
            &stage.id,
            "revoked",
            &Uuid::new_v4().to_string(),
            &setup.session,
            &directory,
            links.reference(),
            &objects,
            &boundaries,
            &DeltaBudget::default()
        )
        .is_err());
        assert!(store.object_set_receipt("budget").unwrap().is_none());
        assert!(store.object_set_receipt("revoked").unwrap().is_none());
    }

    #[test]
    fn retained_partial_data_restores_without_restoring_execution_authority() {
        let graph = graph(1, false);
        let setup = build_setup(&graph, RootLimits::default());
        let temp = temp();
        let store = LocalStore::open(temp.path().join("cas")).unwrap();
        let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let (directory, objects) =
            changed_tree(&store, &stage.id, &graph.file, graph.file.size_bytes);
        let links = topology(&setup.session);
        let retained = delta::retain_partial_tree(
            &store,
            &stage.id,
            "restore",
            &Uuid::new_v4().to_string(),
            &setup.session,
            &directory,
            links.reference(),
            &objects,
            &[inherited_file(&graph, &setup.session), links.clone()],
            &DeltaBudget::default(),
        )
        .unwrap();
        let bytes = serde_json::to_vec(retained.receipt()).unwrap();
        let record: PartialTreeReceipt = serde_json::from_slice(&bytes).unwrap();
        setup.session.invalidate();
        let reopened = LocalStore::open(store.root()).unwrap();
        assert_eq!(
            delta::read_partial_tree(&reopened, &record).unwrap(),
            *retained.plan()
        );
        assert!(setup.session.root().is_err());
        let fresh = setup
            .registry
            .admit(&setup.attempt, "prepared", &graph.root, &control())
            .unwrap();
        for path in &retained.plan().remote_boundaries {
            let handle = fresh
                .resolve_path(path, Instant::now() + Duration::from_secs(5))
                .unwrap();
            assert_eq!(handle.reference(), &path.reference);
            assert!(setup.session.validate_handle(&handle).is_err());
        }
        let mut forged_path = retained.plan().remote_boundaries[0].clone();
        forged_path.reference = ObjectRef::from_bytes(ObjectKind::Blob, b"not on this path");
        assert!(fresh
            .resolve_path(&forged_path, Instant::now() + Duration::from_secs(5))
            .is_err());
        setup.authority.revoked.store(true, Ordering::Release);
        assert!(
            fresh
                .resolve_path(
                    &retained.plan().remote_boundaries[0],
                    Instant::now() + Duration::from_secs(5)
                )
                .is_err(),
            "serialized paths must not restore revoked access"
        );
        let mut forged = record.clone();
        forged.plan = graph.file.clone();
        assert!(delta::read_partial_tree(&reopened, &forged).is_err());
        reopened.release_object_set(&record.local).unwrap();
        assert!(delta::read_partial_tree(&reopened, &record).is_err());
    }
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
    let directory = tempfile::tempdir_in(base).unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}
fn until(mut check: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < end, "bounded root fixture deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn control() -> SourceRequestControl {
    SourceRequestControl::new(Instant::now() + Duration::from_secs(5))
}
fn attempt() -> AttemptBinding {
    AttemptBinding {
        project_id: "project".into(),
        run_id: "run".into(),
        run_epoch: 7,
        owner: OwnerEpoch {
            owner: "host-owner".into(),
            epoch: 9,
        },
        execution_id: Uuid::new_v4().to_string(),
        operation_id: Uuid::new_v4().to_string(),
        view_id: Uuid::new_v4().to_string(),
    }
}
struct Authority {
    revoked: AtomicBool,
    calls: AtomicUsize,
    expected: Mutex<Option<(AttemptBinding, RootBinding)>>,
}
impl Authority {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            revoked: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            expected: Mutex::new(None),
        })
    }
}
impl CurrentAttemptAuthority for Authority {
    fn validate(&self, attempt: &AttemptBinding, root: &RootBinding) -> Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if self.revoked.load(Ordering::Acquire) {
            bail!("private authority diagnostics")
        };
        if let Some((expected, binding)) = &*self.expected.lock().unwrap() {
            if attempt != expected || root != binding {
                bail!("wrong current binding")
            }
        }
        Ok(())
    }
}
struct Source {
    scope: CacheScope,
    objects: Mutex<BTreeMap<ObjectRef, Vec<u8>>>,
    receipt: Mutex<Option<ReplicatedRootReceipt>>,
    calls: Mutex<Vec<ObjectRef>>,
    root_calls: AtomicUsize,
    fail_root: AtomicBool,
    block: Mutex<Option<ObjectRef>>,
    changed: Condvar,
    honor_cancel: AtomicBool,
    started: AtomicUsize,
    saw_cancel: AtomicBool,
}
impl Source {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            scope: CacheScope {
                provider_id: "root-fixture".into(),
                tenant_id: Uuid::new_v4().to_string(),
            },
            objects: Mutex::new(BTreeMap::new()),
            receipt: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            root_calls: AtomicUsize::new(0),
            fail_root: AtomicBool::new(false),
            block: Mutex::new(None),
            changed: Condvar::new(),
            honor_cancel: AtomicBool::new(true),
            started: AtomicUsize::new(0),
            saw_cancel: AtomicBool::new(false),
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
    fn set_root(&self, root: &ObjectRef) {
        let objects = self.objects.lock().unwrap();
        *self.receipt.lock().unwrap() = Some(ReplicatedRootReceipt {
            format_version: 1,
            root_id: "prepared".into(),
            root: root.clone(),
            durability: ReplicatedDurability::Replicated,
            published_at_ms: 1,
            object_count: objects.len() as u64,
            total_bytes: objects.values().map(|v| v.len() as u64).sum(),
        });
    }
    fn release(&self) {
        *self.block.lock().unwrap() = None;
        self.changed.notify_all();
    }
}
impl DirectoryStore for Source {
    fn read(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        self.objects
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .context("fixture object missing")
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
        self.started.fetch_add(1, Ordering::AcqRel);
        let mut block = self.block.lock().unwrap();
        while block.as_ref() == Some(reference) {
            if control.is_cancelled() {
                self.saw_cancel.store(true, Ordering::Release);
                if self.honor_cancel.load(Ordering::Acquire) {
                    control.check()?;
                }
            }
            block = self
                .changed
                .wait_timeout(block, Duration::from_millis(5))
                .unwrap()
                .0;
        }
        drop(block);
        let bytes = self
            .objects
            .lock()
            .unwrap()
            .get(reference)
            .cloned()
            .context("missing private fixture")?;
        for chunk in bytes.chunks(65536) {
            output.write_all(chunk)?;
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
        _root: &str,
        _control: &SourceRequestControl,
    ) -> std::result::Result<ReplicatedRootReceipt, SourceError> {
        self.root_calls.fetch_add(1, Ordering::AcqRel);
        if self.fail_root.load(Ordering::Acquire) {
            return Err(SourceError::RemoteFailed);
        }
        self.receipt
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
}
fn graph(count: usize, large: bool) -> Graph {
    let source = Source::new();
    let mut chunks = Vec::new();
    let file = if large {
        chunks.push(source.put(ObjectKind::Blob, &vec![0x61; 4 * 1024 * 1024]));
        chunks.push(source.put(ObjectKind::Blob, &vec![0x62; 4 * 1024 * 1024]));
        let mut children = vec![chunks[0].clone(); 52];
        children[1] = chunks[1].clone();
        source.metadata(
            ObjectKind::File,
            &FileNode {
                format_version: 1,
                layout: FILE_LAYOUT.into(),
                level: 0,
                logical_size: 208 * 1024 * 1024,
                children,
            },
        )
    } else {
        source.put(ObjectKind::Blob, b"contents")
    };
    let logical = if large {
        208 * 1024 * 1024
    } else {
        file.size_bytes
    };
    let entries: Vec<_> = (0..count)
        .map(|i| {
            DirectoryEntry::new(
                format!("file-{i:08}").as_bytes(),
                EntryNode::File {
                    object: file.clone(),
                    logical_size: logical,
                    mode: 0o644,
                },
            )
            .unwrap()
        })
        .collect();
    let directory = PagedDirectory::new(source.as_ref())
        .build(0o755, entries)
        .unwrap();
    let root = wrap(&source, &directory);
    source.set_root(&root);
    Graph {
        source,
        root,
        directory,
        file,
        chunks,
    }
}
fn wrap(source: &Source, directory: &ObjectRef) -> ObjectRef {
    let policy = source.put(ObjectKind::Blob, b"{}");
    let profile = source.put(ObjectKind::Blob, b"{\"format_version\":1}");
    let hardlinks = source.put(ObjectKind::Blob, b"{\"format_version\":1,\"groups\":[]}");
    let src = source.metadata(
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
            source: src,
            environment: None,
            view_root: directory.clone(),
            profile,
            hardlinks,
            knowledge_cursor: u64::MAX,
        },
    )
}
struct Setup {
    _cache_dir: TempDir,
    registry_dir: TempDir,
    cache: RemoteCache,
    registry: RemoteRootRegistry,
    authority: Arc<Authority>,
    attempt: AttemptBinding,
    session: RemoteSession,
}
fn build_setup(graph: &Graph, limits: RootLimits) -> Setup {
    let cache_dir = temp();
    let registry_dir = temp();
    let cache = RemoteCache::open(
        cache_dir.path(),
        graph.source.clone(),
        CacheLimits::default(),
    )
    .unwrap();
    let authority = Authority::new();
    let attempt = attempt();
    let registry = RemoteRootRegistry::open(
        registry_dir.path(),
        cache.clone(),
        graph.source.clone(),
        authority.clone(),
        limits,
    )
    .unwrap();
    let session = registry
        .admit(&attempt, "prepared", &graph.root, &control())
        .unwrap();
    Setup {
        _cache_dir: cache_dir,
        registry_dir,
        cache,
        registry,
        authority,
        attempt,
        session,
    }
}
fn load(session: &RemoteSession, handle: &ObjectHandle) -> LoadedObject {
    session
        .request(handle, Instant::now() + Duration::from_secs(5))
        .unwrap()
        .wait()
        .unwrap()
}
fn view(session: &RemoteSession) -> LoadedObject {
    let root = load(session, &session.root().unwrap());
    let manifest = root.with_bytes(snapshot::decode_runnable).unwrap().unwrap();
    load(session, &root.child(&manifest.view_root).unwrap())
}
fn lookup(
    session: &RemoteSession,
    directory: LoadedObject,
    name: &[u8],
) -> Option<(LoadedObject, DirectoryEntry)> {
    let root = directory
        .with_bytes(directory::decode_directory_root)
        .unwrap()
        .unwrap();
    let mut loaded = load(session, &directory.child(&root.directory_shard).unwrap());
    loop {
        let shard = loaded.with_bytes(directory::decode_shard).unwrap().unwrap();
        match shard.node {
            ShardNode::Leaf { entries } => {
                return entries
                    .into_iter()
                    .find(|entry| entry.name().unwrap() == name)
                    .map(|entry| (loaded, entry))
            }
            ShardNode::Branch { children } => {
                let prefix = hex::decode(shard.prefix_hex).unwrap();
                if !name.starts_with(&prefix) {
                    return None;
                }
                let edge = if name.len() == prefix.len() {
                    0
                } else {
                    u16::from(name[prefix.len()]) + 1
                };
                let child = children.into_iter().find(|child| child.edge == edge)?;
                loaded = load(session, &loaded.child(&child.shard.reference()).unwrap());
            }
        }
    }
}

#[test]
fn admission_is_fresh_exact_scoped_and_does_not_fetch_any_tree_or_blob() {
    let graph = graph(3, false);
    let setup = build_setup(&graph, RootLimits::default());
    assert_eq!(graph.source.root_calls.load(Ordering::Acquire), 1);
    assert!(graph.source.calls.lock().unwrap().is_empty());
    assert_eq!(setup.cache.stats().unwrap().disk_entries, 0);
    assert_eq!(
        setup
            .registry
            .saved_receipt("prepared")
            .unwrap()
            .unwrap()
            .root,
        graph.root
    );
    let mut foreign = graph.root.clone();
    foreign.digest = "a".repeat(64);
    assert_eq!(
        setup
            .registry
            .admit(&setup.attempt, "prepared", &foreign, &control())
            .unwrap_err(),
        RootError::Conflict
    );
    assert_eq!(
        setup
            .registry
            .admit(&setup.attempt, "changed-root-id", &graph.root, &control())
            .unwrap_err(),
        RootError::Conflict
    );
    setup.authority.revoked.store(true, Ordering::Release);
    let before = graph.source.root_calls.load(Ordering::Acquire);
    assert_eq!(
        setup
            .registry
            .admit(&setup.attempt, "prepared", &graph.root, &control())
            .unwrap_err(),
        RootError::Revoked
    );
    assert_eq!(graph.source.root_calls.load(Ordering::Acquire), before);
}

#[test]
fn handles_require_actual_parent_edges_and_the_same_live_session() {
    let graph = graph(2, false);
    let setup = build_setup(&graph, RootLimits::default());
    let root = load(&setup.session, &setup.session.root().unwrap());
    let unrelated = graph.source.put(ObjectKind::Blob, b"not part of this root");
    assert_eq!(
        root.child(&unrelated).unwrap_err(),
        RootError::ForeignHandle
    );
    let second = setup
        .registry
        .admit(&setup.attempt, "prepared", &graph.root, &control())
        .unwrap();
    let handle = root.child(&graph.directory).unwrap();
    assert_eq!(
        second
            .request(&handle, Instant::now() + Duration::from_secs(1))
            .unwrap_err(),
        RootError::ForeignHandle
    );
    let before = graph.source.calls.lock().unwrap().len();
    setup.session.invalidate();
    assert_eq!(
        root.with_bytes(|bytes| bytes.len()).unwrap_err(),
        RootError::Revoked
    );
    assert_eq!(
        root.child(&graph.directory).unwrap_err(),
        RootError::Revoked
    );
    assert_eq!(graph.source.calls.lock().unwrap().len(), before);
}

struct Counter(AtomicUsize);
impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}
#[test]
fn revocation_wakes_the_future_and_cancels_interest_without_forging_transport_settlement() {
    let graph = graph(1, false);
    let setup = build_setup(&graph, RootLimits::default());
    *graph.source.block.lock().unwrap() = Some(graph.root.clone());
    graph.source.honor_cancel.store(false, Ordering::Release);
    let mut ticket = setup
        .session
        .request(
            &setup.session.root().unwrap(),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    let counter = Arc::new(Counter(AtomicUsize::new(0)));
    let waker = Waker::from(counter.clone());
    let mut context = TaskContext::from_waker(&waker);
    assert!(Pin::new(&mut ticket).poll(&mut context).is_pending());
    until(|| graph.source.started.load(Ordering::Acquire) == 1);
    setup.authority.revoked.store(true, Ordering::Release);
    setup.session.invalidate();
    until(|| graph.source.saw_cancel.load(Ordering::Acquire));
    assert!(counter.0.load(Ordering::Acquire) > 0);
    assert_eq!(
        ticket.try_result().unwrap().unwrap_err(),
        RootError::Revoked
    );
    assert_eq!(setup.cache.stats().unwrap().inflight_jobs, 1);
    graph.source.release();
    until(|| setup.cache.stats().unwrap().inflight_jobs == 0);
    assert_eq!(setup.registry.stats().unwrap().borrowed_bytes, 0);
    assert_eq!(setup.registry.stats().unwrap().availability_hints, 0);
}

#[test]
fn semantic_directory_counts_and_file_lengths_are_checked_before_child_bytes_escape() {
    let graph = graph(1, true);
    let mut forged = directory::decode_directory_root(
        &DirectoryStore::read(graph.source.as_ref(), &graph.directory).unwrap(),
    )
    .unwrap();
    forged.entry_count += 1;
    let directory = graph.source.metadata(ObjectKind::Directory, &forged);
    let root = wrap(graph.source.as_ref(), &directory);
    graph.source.set_root(&root);
    let malformed = Graph {
        source: graph.source,
        root,
        directory,
        file: graph.file,
        chunks: graph.chunks,
    };
    let setup = build_setup(&malformed, RootLimits::default());
    let directory = view(&setup.session);
    let child = directory.child(&forged.directory_shard).unwrap();
    assert_eq!(
        setup
            .session
            .request(&child, Instant::now() + Duration::from_secs(2))
            .unwrap()
            .wait()
            .unwrap_err(),
        RootError::Corrupt
    );
    let source = Source::new();
    let blob = source.put(ObjectKind::Blob, b"data");
    let file = source.metadata(
        ObjectKind::File,
        &FileNode {
            format_version: 1,
            layout: FILE_LAYOUT.into(),
            level: 0,
            logical_size: 4,
            children: vec![blob],
        },
    );
    let directory = PagedDirectory::new(source.as_ref())
        .build(
            0o755,
            vec![
                DirectoryEntry::new(
                    b"a",
                    EntryNode::File {
                        object: file.clone(),
                        logical_size: 4,
                        mode: 0o644,
                    },
                )
                .unwrap(),
                DirectoryEntry::new(
                    b"b",
                    EntryNode::File {
                        object: file.clone(),
                        logical_size: 5,
                        mode: 0o644,
                    },
                )
                .unwrap(),
            ],
        )
        .unwrap();
    let root = wrap(source.as_ref(), &directory);
    source.set_root(&root);
    let malformed = Graph {
        source,
        root,
        directory,
        file: file.clone(),
        chunks: vec![],
    };
    let setup = build_setup(&malformed, RootLimits::default());
    let (leaf, _) = lookup(&setup.session, view(&setup.session), b"a").unwrap();
    assert_eq!(
        setup
            .session
            .request(
                &leaf.child(&file).unwrap(),
                Instant::now() + Duration::from_secs(2)
            )
            .unwrap()
            .wait()
            .unwrap_err(),
        RootError::Corrupt,
        "the second alias must be validated too"
    );
}

#[test]
fn quotas_bound_proof_handles_loaded_bytes_depth_sessions_and_pending_loads() {
    let graph = graph(2, false);
    let setup = build_setup(
        &graph,
        RootLimits {
            max_sessions: 1,
            max_handles: 2,
            max_loaded_objects: 1,
            max_pending_loads: 1,
            max_depth: 1,
            ..Default::default()
        },
    );
    assert_eq!(
        setup
            .registry
            .admit(&setup.attempt, "prepared", &graph.root, &control())
            .unwrap_err(),
        RootError::Busy
    );
    let handle = setup.session.root().unwrap();
    let mut first = setup
        .session
        .request(&handle, Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        setup
            .session
            .request(&handle, Instant::now() + Duration::from_secs(2))
            .unwrap_err(),
        RootError::Busy
    );
    let root = loop {
        if let Some(value) = first.try_result() {
            break value.unwrap();
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    drop(first);
    assert_eq!(
        setup
            .session
            .request(&handle, Instant::now() + Duration::from_secs(2))
            .unwrap()
            .wait()
            .unwrap_err(),
        RootError::Busy
    );
    let child = root.child(&graph.directory).unwrap();
    assert_eq!(root.child(&graph.directory).unwrap_err(), RootError::Busy);
    drop(child);
    drop(root);
    drop(handle);
    assert_eq!(setup.registry.stats().unwrap().handles, 0);
    assert_eq!(setup.registry.stats().unwrap().loaded_objects, 0);
    assert_eq!(setup.registry.stats().unwrap().borrowed_bytes, 0);
    let depth_setup = build_setup(
        &graph,
        RootLimits {
            max_depth: 1,
            ..Default::default()
        },
    );
    let handle = depth_setup.session.root().unwrap();
    let root = load(&depth_setup.session, &handle);
    let child = root.child(&graph.directory).unwrap();
    drop(root);
    let directory = load(&depth_setup.session, &child);
    let shard = directory
        .with_bytes(directory::decode_directory_root)
        .unwrap()
        .unwrap()
        .directory_shard;
    assert_eq!(directory.child(&shard).unwrap_err(), RootError::Busy);
    drop(directory);
    drop(child);
    drop(handle);
    assert_eq!(depth_setup.registry.stats().unwrap().handles, 0);
    assert_eq!(depth_setup.registry.stats().unwrap().loaded_objects, 0);
    assert_eq!(depth_setup.registry.stats().unwrap().borrowed_bytes, 0);
}

#[test]
fn cancelled_handle_tickets_keep_cache_transport_owned_until_ack() {
    let graph = graph(1, false);
    let setup = build_setup(&graph, RootLimits::default());
    *graph.source.block.lock().unwrap() = Some(graph.root.clone());
    graph.source.honor_cancel.store(false, Ordering::Release);
    let mut ticket = setup
        .session
        .request(
            &setup.session.root().unwrap(),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    until(|| graph.source.started.load(Ordering::Acquire) == 1);
    ticket.cancel();
    assert_eq!(
        ticket.try_result().unwrap().unwrap_err(),
        RootError::Cancelled
    );
    drop(ticket);
    assert_eq!(setup.registry.stats().unwrap().pending_loads, 0);
    assert_eq!(setup.cache.stats().unwrap().inflight_jobs, 1);
    graph.source.release();
    until(|| setup.cache.stats().unwrap().inflight_jobs == 0);
}

#[test]
fn saved_receipts_and_evicted_availability_hints_never_restore_attempt_authorization() {
    let graph = graph(2, false);
    let setup = build_setup(
        &graph,
        RootLimits {
            max_availability_hints: 1,
            ..Default::default()
        },
    );
    let directory = view(&setup.session);
    until(|| setup.registry.stats().unwrap().availability_hints == 1);
    assert_eq!(setup.registry.stats().unwrap().saved_roots, 1);
    drop(directory);
    graph.source.fail_root.store(true, Ordering::Release);
    let before = graph.source.calls.lock().unwrap().len();
    assert!(setup.registry.saved_receipt("prepared").unwrap().is_some());
    assert_eq!(
        setup
            .registry
            .admit(&setup.attempt, "prepared", &graph.root, &control())
            .unwrap_err(),
        RootError::SourceFailed
    );
    assert_eq!(graph.source.calls.lock().unwrap().len(), before);
}

#[test]
fn legacy_directory_and_source_versions_require_an_explicit_bulk_path() {
    let graph = graph(1, false);
    let source = graph.source.clone();
    let legacy = source.metadata(
        ObjectKind::Directory,
        &serde_json::json!({"format_version":1,"mode":493,"entries":[]}),
    );
    let root = wrap(&source, &legacy);
    source.set_root(&root);
    let graph = Graph {
        source,
        root,
        directory: legacy,
        file: graph.file,
        chunks: vec![],
    };
    let setup = build_setup(&graph, RootLimits::default());
    let root = load(&setup.session, &setup.session.root().unwrap());
    let child = root.child(&graph.directory).unwrap();
    assert_eq!(
        setup
            .session
            .request(&child, Instant::now() + Duration::from_secs(2))
            .unwrap()
            .wait()
            .unwrap_err(),
        RootError::Unsupported
    );
}

#[test]
fn swapped_radix_pages_and_aliased_file_subtrees_fail_before_exposure() {
    let graph = graph(2000, false);
    let source = graph.source.clone();
    let mut root = directory::decode_directory_root(
        &DirectoryStore::read(source.as_ref(), &graph.directory).unwrap(),
    )
    .unwrap();
    let mut shard = directory::decode_shard(
        &DirectoryStore::read(source.as_ref(), &root.directory_shard).unwrap(),
    )
    .unwrap();
    let ShardNode::Branch { children } = &mut shard.node else {
        panic!("fixture must branch")
    };
    let other = children[1].shard.clone();
    children[1].shard = children[0].shard.clone();
    children[0].shard = other;
    root.directory_shard = source.metadata(ObjectKind::DirectoryShard, &shard);
    let directory = source.metadata(ObjectKind::Directory, &root);
    let runnable = wrap(&source, &directory);
    source.set_root(&runnable);
    let malformed = Graph {
        source,
        root: runnable,
        directory,
        file: graph.file,
        chunks: vec![],
    };
    let setup = build_setup(&malformed, RootLimits::default());
    let directory = view(&setup.session);
    let parent = load(
        &setup.session,
        &directory.child(&root.directory_shard).unwrap(),
    );
    assert_eq!(
        setup
            .session
            .request(
                &parent.child_at(0).unwrap(),
                Instant::now() + Duration::from_secs(2)
            )
            .unwrap()
            .wait()
            .unwrap_err(),
        RootError::Corrupt
    );

    let source = Source::new();
    let blob = source.put(ObjectKind::Blob, &vec![0x33; 4 * 1024 * 1024]);
    let lower = source.metadata(
        ObjectKind::File,
        &FileNode {
            format_version: 1,
            layout: FILE_LAYOUT.into(),
            level: 0,
            logical_size: 1024 * 1024 * 1024,
            children: vec![blob; 256],
        },
    );
    let higher = source.metadata(
        ObjectKind::File,
        &FileNode {
            format_version: 1,
            layout: FILE_LAYOUT.into(),
            level: 1,
            logical_size: 1024 * 1024 * 1024 + 4 * 1024 * 1024,
            children: vec![lower.clone(), lower],
        },
    );
    let directory = PagedDirectory::new(source.as_ref())
        .build(
            0o755,
            vec![DirectoryEntry::new(
                b"file",
                EntryNode::File {
                    object: higher.clone(),
                    logical_size: 1024 * 1024 * 1024 + 4 * 1024 * 1024,
                    mode: 0o644,
                },
            )
            .unwrap()],
        )
        .unwrap();
    let runnable = wrap(&source, &directory);
    source.set_root(&runnable);
    let malformed = Graph {
        source,
        root: runnable,
        directory,
        file: higher.clone(),
        chunks: vec![],
    };
    let setup = build_setup(&malformed, RootLimits::default());
    let (leaf, _) = lookup(&setup.session, view(&setup.session), b"file").unwrap();
    let file = load(&setup.session, &leaf.child(&higher).unwrap());
    assert_eq!(
        setup
            .session
            .request(
                &file.child_at(0).unwrap(),
                Instant::now() + Duration::from_secs(2)
            )
            .unwrap()
            .wait()
            .unwrap_err(),
        RootError::Corrupt,
        "same descriptor at a shorter second edge cannot escape via its valid first alias"
    );
}

#[test]
fn actual_source_and_environment_versions_are_required_not_just_reference_kinds() {
    for legacy_kind in [ObjectKind::SourceRoot, ObjectKind::EnvironmentRoot] {
        let graph = graph(1, false);
        let source = graph.source.clone();
        let mut runnable =
            snapshot::decode_runnable(&DirectoryStore::read(source.as_ref(), &graph.root).unwrap())
                .unwrap();
        let legacy = if legacy_kind == ObjectKind::SourceRoot {
            source.metadata(legacy_kind,&serde_json::json!({"format_version":1,"consistency":"quiesced","directory":graph.directory,"parents":[]}))
        } else {
            source.metadata(legacy_kind,&serde_json::json!({"format_version":1,"directory":graph.directory,"platform":{"os":"linux"},"installer_inputs":[],"provenance":{}}))
        };
        if legacy_kind == ObjectKind::SourceRoot {
            runnable.source = legacy.clone()
        } else {
            runnable.environment = Some(legacy.clone())
        }
        let root = source.metadata(ObjectKind::RunnableRoot, &runnable);
        source.set_root(&root);
        let graph = Graph {
            source,
            root,
            directory: graph.directory,
            file: graph.file,
            chunks: vec![],
        };
        let setup = build_setup(&graph, RootLimits::default());
        let root = load(&setup.session, &setup.session.root().unwrap());
        assert_eq!(
            setup
                .session
                .request(
                    &root.child(&legacy).unwrap(),
                    Instant::now() + Duration::from_secs(2)
                )
                .unwrap()
                .wait()
                .unwrap_err(),
            RootError::Unsupported
        );
    }
}

#[test]
fn proof_byte_and_loaded_byte_quotas_have_no_unaccounted_successes() {
    let graph = graph(1, false);
    let setup = build_setup(
        &graph,
        RootLimits {
            max_borrowed_bytes: 1,
            ..Default::default()
        },
    );
    let handle = setup.session.root().unwrap();
    assert_eq!(
        setup
            .session
            .request(&handle, Instant::now() + Duration::from_secs(2))
            .unwrap()
            .wait()
            .unwrap_err(),
        RootError::Busy
    );
    assert_eq!(setup.registry.stats().unwrap().borrowed_bytes, 0);
    let setup = build_setup(
        &graph,
        RootLimits {
            max_proof_bytes: 1,
            ..Default::default()
        },
    );
    let root = load(&setup.session, &setup.session.root().unwrap());
    assert_eq!(root.child(&graph.directory).unwrap_err(), RootError::Busy);
    assert_eq!(setup.registry.stats().unwrap().handles, 1);
}

#[test]
fn restart_requires_fresh_authorization_and_live_proofs_keep_registry_ownership() {
    let graph = graph(2, false);
    let setup = build_setup(&graph, RootLimits::default());
    let root = load(&setup.session, &setup.session.root().unwrap());
    until(|| setup.registry.stats().unwrap().availability_hints > 0);
    let Setup {
        _cache_dir,
        registry_dir,
        cache,
        registry,
        authority,
        attempt,
        session,
    } = setup;
    let path = registry.metadata_root().to_path_buf();
    drop(registry);
    drop(session);
    assert!(
        matches!(
            RemoteRootRegistry::open(
                registry_dir.path(),
                cache.clone(),
                graph.source.clone(),
                authority.clone(),
                RootLimits::default()
            ),
            Err(RootError::Busy)
        ),
        "borrowed proof keeps registry ownership"
    );
    drop(root);
    let registry = loop {
        match RemoteRootRegistry::open(
            registry_dir.path(),
            cache.clone(),
            graph.source.clone(),
            authority.clone(),
            RootLimits::default(),
        ) {
            Ok(registry) => break registry,
            Err(RootError::Busy) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => panic!("{error}"),
        }
    };
    assert!(registry.saved_receipt("prepared").unwrap().is_some());
    assert_eq!(registry.stats().unwrap().sessions, 0);
    graph.source.fail_root.store(true, Ordering::Release);
    assert_eq!(
        registry
            .admit(&attempt, "prepared", &graph.root, &control())
            .unwrap_err(),
        RootError::SourceFailed
    );
    assert_eq!(registry.stats().unwrap().sessions, 0);
    graph.source.fail_root.store(false, Ordering::Release);
    let session = registry
        .admit(&attempt, "prepared", &graph.root, &control())
        .unwrap();
    let before = graph.source.calls.lock().unwrap().len();
    load(&session, &session.root().unwrap());
    assert_eq!(
        graph.source.calls.lock().unwrap().len(),
        before,
        "healthy cached object is local after fresh root admission"
    );
    drop(session);
    drop(registry);
    until(|| {
        let path = path.join("registry.lock");
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).is_ok()
    });
    std::os::unix::fs::symlink("scope.json", path.join("unrecognized-link")).unwrap();
    assert!(RemoteRootRegistry::open(
        registry_dir.path(),
        cache.clone(),
        graph.source.clone(),
        authority,
        RootLimits::default()
    )
    .is_err());
}

#[test]
fn evicted_pages_require_the_original_proof_path_and_are_refetched_safely() {
    let graph = graph(2000, false);
    let cache_dir = temp();
    let registry_dir = temp();
    let cache = RemoteCache::open(
        cache_dir.path(),
        graph.source.clone(),
        CacheLimits {
            disk_quota_bytes: 96 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let authority = Authority::new();
    let registry = RemoteRootRegistry::open(
        registry_dir.path(),
        cache.clone(),
        graph.source.clone(),
        authority,
        RootLimits::default(),
    )
    .unwrap();
    let session = registry
        .admit(&attempt(), "prepared", &graph.root, &control())
        .unwrap();
    let mut first = None;
    for index in [0, 250, 500, 750, 1000, 1250, 1500, 1750] {
        let (leaf, _) = lookup(
            &session,
            view(&session),
            format!("file-{index:08}").as_bytes(),
        )
        .unwrap();
        if first.is_none() {
            first = Some(leaf.reference().clone())
        }
        drop(leaf);
    }
    assert!(cache.stats().unwrap().evictions > 0);
    let root = load(&session, &session.root().unwrap());
    assert_eq!(
        root.child(first.as_ref().unwrap()).unwrap_err(),
        RootError::ForeignHandle,
        "cached/reachable descendant is not an immediate declared root edge"
    );
    drop(root);
    let (leaf, _) = lookup(&session, view(&session), b"file-00000000").unwrap();
    assert_eq!(Some(leaf.reference()), first.as_ref());
    assert!(
        graph
            .source
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|reference| Some(*reference) == first.as_ref())
            .count()
            >= 2
    );
    assert!(cache.stats().unwrap().disk_bytes <= 96 * 1024);
}

#[test]
fn scope_receipt_conflicts_and_persisted_root_caps_fail_closed() {
    let graph = graph(1, false);
    let setup = build_setup(
        &graph,
        RootLimits {
            max_saved_roots: 1,
            ..Default::default()
        },
    );
    let foreign = Source::new();
    let before = graph.source.root_calls.load(Ordering::Acquire);
    assert!(matches!(
        RemoteRootRegistry::open(
            setup.registry_dir.path(),
            setup.cache.clone(),
            foreign,
            setup.authority.clone(),
            RootLimits::default()
        ),
        Err(RootError::Conflict)
    ));
    assert_eq!(graph.source.root_calls.load(Ordering::Acquire), before);
    graph
        .source
        .receipt
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .published_at_ms = 2;
    assert_eq!(
        setup
            .registry
            .admit(&setup.attempt, "prepared", &graph.root, &control())
            .unwrap_err(),
        RootError::Conflict
    );
    assert_eq!(
        setup
            .registry
            .saved_receipt("prepared")
            .unwrap()
            .unwrap()
            .published_at_ms,
        1
    );
    {
        let mut receipt = graph.source.receipt.lock().unwrap();
        let receipt = receipt.as_mut().unwrap();
        receipt.published_at_ms = 1;
        receipt.root_id = "next".into();
    }
    assert_eq!(
        setup
            .registry
            .admit(&setup.attempt, "next", &graph.root, &control())
            .unwrap_err(),
        RootError::Busy
    );
    assert_eq!(setup.registry.stats().unwrap().saved_roots, 1);
    assert!(setup.registry.saved_receipt("next").unwrap().is_none());
}

fn crash_graph() -> Graph {
    let mut graph = graph(1, false);
    Arc::get_mut(&mut graph.source).unwrap().scope = CacheScope {
        provider_id: "root-crash-fixture".into(),
        tenant_id: "public-deterministic".into(),
    };
    graph
}
#[test]
fn remote_root_abort_child() {
    let Some(base) = std::env::var_os("RUDDER_REMOTE_ROOT_ABORT_FIXTURE") else {
        return;
    };
    let base = PathBuf::from(base);
    let graph = crash_graph();
    let cache = RemoteCache::open(
        base.join("cache"),
        graph.source.clone(),
        CacheLimits::default(),
    )
    .unwrap();
    let registry = RemoteRootRegistry::open(
        base.join("registry"),
        cache,
        graph.source.clone(),
        Authority::new(),
        RootLimits::default(),
    )
    .unwrap();
    let session = registry
        .admit(&attempt(), "prepared", &graph.root, &control())
        .unwrap();
    let _root = load(&session, &session.root().unwrap());
    until(|| registry.stats().unwrap().availability_hints == 1);
    unsafe { libc::_exit(79) }
}
#[test]
fn abrupt_process_exit_retains_hints_but_restart_requires_new_root_authorization() {
    let base = temp();
    for name in ["cache", "registry"] {
        fs::create_dir(base.path().join(name)).unwrap();
        fs::set_permissions(base.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "remote_root_abort_child", "--nocapture"])
        .env("RUDDER_REMOTE_ROOT_ABORT_FIXTURE", base.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(79));
    let graph = crash_graph();
    let cache = RemoteCache::open(
        base.path().join("cache"),
        graph.source.clone(),
        CacheLimits::default(),
    )
    .unwrap();
    assert_eq!(cache.stats().unwrap().disk_entries, 1);
    let registry = RemoteRootRegistry::open(
        base.path().join("registry"),
        cache.clone(),
        graph.source.clone(),
        Authority::new(),
        RootLimits::default(),
    )
    .unwrap();
    assert!(registry.saved_receipt("prepared").unwrap().is_some());
    assert_eq!(registry.stats().unwrap().availability_hints, 1);
    assert_eq!(registry.stats().unwrap().sessions, 0);
    graph.source.fail_root.store(true, Ordering::Release);
    assert_eq!(
        registry
            .admit(&attempt(), "prepared", &graph.root, &control())
            .unwrap_err(),
        RootError::SourceFailed
    );
    assert!(graph.source.calls.lock().unwrap().is_empty());
    graph.source.fail_root.store(false, Ordering::Release);
    let session = registry
        .admit(&attempt(), "prepared", &graph.root, &control())
        .unwrap();
    load(&session, &session.root().unwrap());
    assert_eq!(cache.stats().unwrap().remote_fetches, 0);
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
                                        source.receipt.lock().unwrap().as_ref().unwrap(),
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

#[test]
fn actual_helper_http_lazy_lookup_in_100k_directory_fetches_only_two_4mib_ranges() {
    let start = Instant::now();
    let graph = graph(100000, true);
    let fixture_ms = start.elapsed().as_millis();
    let http = Http::new(graph.source.clone());
    let files = temp();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let bundle = files.path().join("fetch.mjs");
    let output = Command::new(root.join("node_modules/.bin/esbuild"))
        .current_dir(root)
        .arg("src/fabric-fetch.ts")
        .args(["--bundle", "--platform=node", "--format=esm"])
        .arg(format!("--outfile={}", bundle.display()))
        .output()
        .expect("npm ci must precede real Cloud source tests");
    assert!(output.status.success());
    let script = files.path().join("helper.mjs");
    fs::write(
        &script,
        format!(
            "import{{runFabricFetchHelper}}from {};await runFabricFetchHelper();",
            serde_json::to_string(&format!("file://{}", bundle.display())).unwrap()
        ),
    )
    .unwrap();
    let node = Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .expect("Node22+ required");
    let node = PathBuf::from(String::from_utf8(node.stdout).unwrap().trim());
    let scope = graph.source.scope.clone();
    let mut config = NodeSourceConfig::new(
        scope,
        http.url.clone(),
        BTreeMap::from([("Authorization".into(), "Bearer public-root-fixture".into())]),
    );
    config.transport = TransportProfile::LoopbackHttpTest;
    let source = Arc::new(NodeRemoteSource::spawn(node, &script, config).unwrap());
    let cache_dir = temp();
    let registry_dir = temp();
    let cache =
        RemoteCache::open(cache_dir.path(), source.clone(), CacheLimits::default()).unwrap();
    let authority = Authority::new();
    let registry = RemoteRootRegistry::open(
        registry_dir.path(),
        cache.clone(),
        source.clone(),
        authority.clone(),
        RootLimits::default(),
    )
    .unwrap();
    let attempt = attempt();
    let binding = RootBinding {
        scope: graph.source.scope.clone(),
        root_id: "prepared".into(),
        root: graph.root.clone(),
    };
    *authority.expected.lock().unwrap() = Some((attempt.clone(), binding));
    let now = Instant::now();
    let session = registry
        .admit(&attempt, "prepared", &graph.root, &control())
        .unwrap();
    let admission_ms = now.elapsed().as_millis();
    assert_eq!(http.roots.load(Ordering::Acquire), 1);
    assert!(http.objects.lock().unwrap().is_empty());
    assert_eq!(cache.stats().unwrap().disk_entries, 0);
    let now = Instant::now();
    let (leaf, entry) = lookup(&session, view(&session), b"file-00050000").unwrap();
    let EntryNode::File {
        object,
        logical_size,
        ..
    } = entry.node
    else {
        panic!("file expected")
    };
    assert_eq!(logical_size, 208 * 1024 * 1024);
    let file = load(&session, &leaf.child(&object).unwrap());
    assert_eq!(
        http.objects
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.kind == ObjectKind::Blob)
            .count(),
        0
    );
    let first = load(&session, &file.child_at(0).unwrap());
    assert_eq!(
        first.with_bytes(|b| b[1234..1241].to_vec()).unwrap(),
        b"aaaaaaa"
    );
    let second = load(&session, &file.child_at(1).unwrap());
    assert_eq!(
        second.with_bytes(|b| b[999..1006].to_vec()).unwrap(),
        b"bbbbbbb"
    );
    let range_ms = now.elapsed().as_millis();
    let objects = http.objects.lock().unwrap().clone();
    let chunks: Vec<_> = objects
        .iter()
        .filter(|r| r.kind == ObjectKind::Blob)
        .collect();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0], &graph.chunks[0]);
    assert_eq!(chunks[1], &graph.chunks[1]);
    assert!(objects.len() < 20);
    assert!(cache.stats().unwrap().disk_bytes < 9 * 1024 * 1024);
    eprintln!(
        "{}",
        serde_json::json!({"event":"remote-root-lazy-http","entries":100000,"logical_file_bytes":logical_size,"fixture_ms":fixture_ms,"fresh_admission_ms":admission_ms,"lookup_and_two_ranges_ms":range_ms,"root_receipt_requests":http.roots.load(Ordering::Acquire),"metadata_requests":objects.len()-chunks.len(),"chunk_requests":chunks.len(),"downloaded_chunk_bytes":chunks.iter().map(|r|r.size_bytes).sum::<u64>(),"partial_cache_bytes":cache.stats().unwrap().disk_bytes})
    );
    session.invalidate();
    assert_eq!(
        first.with_bytes(|b| b.len()).unwrap_err(),
        RootError::Revoked
    );
    source.terminate();
    until(|| source.stats().unwrap().child_exited);
}
