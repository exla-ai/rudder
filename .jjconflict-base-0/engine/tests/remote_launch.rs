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
    snapshot::{EnvironmentV2, RunnableRoot, SourceV2},
    store::{ImportPolicy, LocalStore},
    vfs::{
        exclusion_policy_digest,
        remote_launch::{self, RemoteLaunchRequest},
        rpc::{AttemptContext, ObservationBinding},
        NodeKind, ObservationBudget, PartialCaptureBudget, PartialCaptureRequest,
        RemoteFreezeRequest, RemoteOptions, VfsManager, VfsView,
    },
    workspace::WorkspaceProfile,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Default)]
struct Authority(AtomicBool);
impl CurrentAttemptAuthority for Authority {
    fn validate(&self, _: &AttemptBinding, _: &RootBinding) -> Result<()> {
        if self.0.load(Ordering::Acquire) {
            bail!("fixture revoked")
        };
        Ok(())
    }
}
struct Source {
    scope: CacheScope,
    objects: Mutex<BTreeMap<ObjectRef, Vec<u8>>>,
    receipt: Mutex<Option<ReplicatedRootReceipt>>,
    calls: Mutex<Vec<ObjectRef>>,
    admissions: AtomicUsize,
    root_unavailable: AtomicBool,
    delay_ms: AtomicU64,
    held_fetch: Arc<AtomicBool>,
    fetch_entered: Arc<AtomicBool>,
    revoke_after: AtomicUsize,
    authority: Arc<Authority>,
}
impl Source {
    fn new(authority: Arc<Authority>) -> Arc<Self> {
        Arc::new(Self {
            scope: CacheScope {
                provider_id: "remote-launch-fixture".into(),
                tenant_id: Uuid::new_v4().to_string(),
            },
            objects: Mutex::new(BTreeMap::new()),
            receipt: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            admissions: AtomicUsize::new(0),
            root_unavailable: AtomicBool::new(false),
            delay_ms: AtomicU64::new(0),
            held_fetch: Arc::new(AtomicBool::new(false)),
            fetch_entered: Arc::new(AtomicBool::new(false)),
            revoke_after: AtomicUsize::new(usize::MAX),
            authority,
        })
    }
    fn put(&self, kind: ObjectKind, bytes: &[u8]) -> ObjectRef {
        let reference = ObjectRef::from_bytes(kind, bytes);
        self.objects
            .lock()
            .unwrap()
            .insert(reference.clone(), bytes.into());
        reference
    }
    fn metadata(&self, kind: ObjectKind, value: &impl Serialize) -> ObjectRef {
        self.put(kind, &canonical_json(value).unwrap())
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
        let count = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(reference.clone());
            calls.len()
        };
        // Model a transport that ignores cancellation until actual settlement.
        // The fuse bounds the fixture even when an assertion unwinds.
        if self.held_fetch.load(Ordering::Acquire) {
            self.fetch_entered.store(true, Ordering::Release);
            let fuse = Instant::now() + Duration::from_secs(3);
            while self.held_fetch.load(Ordering::Acquire) && Instant::now() < fuse {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let until = Instant::now() + Duration::from_millis(self.delay_ms.load(Ordering::Acquire));
        while Instant::now() < until {
            control.check()?;
            std::thread::sleep(Duration::from_millis(1));
        }
        for chunk in DirectoryStore::read(self, reference)?.chunks(65536) {
            control.check()?;
            output.write_all(chunk)?;
        }
        if count == self.revoke_after.load(Ordering::Acquire) {
            self.authority.0.store(true, Ordering::Release);
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
        _: &str,
        _: &SourceRequestControl,
    ) -> std::result::Result<ReplicatedRootReceipt, SourceError> {
        self.admissions.fetch_add(1, Ordering::AcqRel);
        if self.root_unavailable.load(Ordering::Acquire) {
            return Err(SourceError::RemoteFailed);
        }
        self.receipt
            .lock()
            .unwrap()
            .clone()
            .ok_or(SourceError::RemoteFailed)
    }
}
#[derive(Default)]
struct Shape {
    count: usize,
    nested_directory: bool,
    different_directory: bool,
    environment: bool,
    partitions: bool,
    hardlinks: bool,
    noncanonical_policy: bool,
}
struct Fixture {
    temp: tempfile::TempDir,
    source: Arc<Source>,
    authority: Arc<Authority>,
    _cache: RemoteCache,
    registry: RemoteRootRegistry,
    store: LocalStore,
    request: RemoteLaunchRequest,
    payload: ObjectRef,
    file: ObjectRef,
}
impl Fixture {
    fn new(shape: Shape) -> Self {
        let base = if std::path::Path::new("/workspace").is_dir() {
            PathBuf::from("/workspace")
        } else {
            std::env::temp_dir()
        };
        let temp = tempfile::tempdir_in(base).unwrap();
        let authority = Arc::new(Authority::default());
        let source = Source::new(authority.clone());
        let payload = source.put(ObjectKind::Blob, &vec![b'p'; CHUNK_BYTES]);
        let file = source.metadata(
            ObjectKind::File,
            &FileNode {
                format_version: 1,
                layout: FILE_LAYOUT.into(),
                level: 0,
                logical_size: 52 * CHUNK_BYTES as u64,
                children: vec![payload.clone(); 52],
            },
        );
        let mut entries = (0..shape.count)
            .map(|index| {
                DirectoryEntry::new(
                    format!("file-{index:08}").as_bytes(),
                    EntryNode::File {
                        object: file.clone(),
                        logical_size: 52 * CHUNK_BYTES as u64,
                        mode: 0o644,
                    },
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        if shape.nested_directory {
            let child = PagedDirectory::new(source.as_ref())
                .build(0o711, std::iter::empty())
                .unwrap();
            entries.push(
                DirectoryEntry::new(b"nested", EntryNode::Directory { object: child }).unwrap(),
            );
        }
        let directory = PagedDirectory::new(source.as_ref())
            .build(0o755, entries)
            .unwrap();
        let seed = PagedDirectory::new(source.as_ref())
            .build(0o700, [])
            .unwrap();
        let mut profile = WorkspaceProfile::source_only();
        if shape.partitions {
            profile.scratch.push(vec![b"scratch".to_vec()]);
        }
        let profile = source.metadata(ObjectKind::Blob, &profile);
        let policy = source.put(
            ObjectKind::Blob,
            if shape.noncanonical_policy {
                b"{ \"format_version\":1}"
            } else {
                b"{\"format_version\":1}"
            },
        );
        let accepted_source = source.metadata(
            ObjectKind::SourceRoot,
            &SourceV2 {
                format_version: 2,
                consistency: CheckpointConsistency::Quiesced,
                directory: if shape.different_directory {
                    seed.clone()
                } else {
                    directory.clone()
                },
                projection_policy: policy.clone(),
            },
        );
        let hardlinks = source.metadata(
            ObjectKind::Blob,
            &if shape.hardlinks {
                serde_json::json!({"format_version":1,"groups":[{"paths_hex":[["61"],["62"]]}]})
            } else {
                serde_json::json!({"format_version":1,"groups":[]})
            },
        );
        let environment = shape.environment.then(|| {
            source.metadata(
                ObjectKind::EnvironmentRoot,
                &EnvironmentV2 {
                    format_version: 2,
                    directory: seed.clone(),
                    platform: BTreeMap::from([("os".into(), "linux".into())]),
                    installer_inputs: vec![],
                    provenance: BTreeMap::new(),
                },
            )
        });
        let root = source.metadata(
            ObjectKind::RunnableRoot,
            &RunnableRoot {
                format_version: 1,
                consistency: CheckpointConsistency::Quiesced,
                source: accepted_source.clone(),
                environment,
                view_root: directory,
                profile: profile.clone(),
                hardlinks,
                knowledge_cursor: 0,
            },
        );
        let (object_count, total_bytes) = {
            let objects = source.objects.lock().unwrap();
            (
                objects.len() as u64,
                objects.values().map(|bytes| bytes.len() as u64).sum(),
            )
        };
        let receipt = ReplicatedRootReceipt {
            format_version: 1,
            root_id: "fixture-root".into(),
            root: root.clone(),
            durability: ReplicatedDurability::Replicated,
            published_at_ms: 1,
            object_count,
            total_bytes,
        };
        *source.receipt.lock().unwrap() = Some(receipt);
        for name in ["cache", "registry"] {
            fs::create_dir(temp.path().join(name)).unwrap();
            fs::set_permissions(temp.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let cache = RemoteCache::open(
            temp.path().join("cache"),
            source.clone(),
            CacheLimits::default(),
        )
        .unwrap();
        let registry = RemoteRootRegistry::open(
            temp.path().join("registry"),
            cache.clone(),
            source.clone(),
            authority.clone(),
            RootLimits::default(),
        )
        .unwrap();
        let attempt = AttemptBinding {
            project_id: "project".into(),
            run_id: "run".into(),
            run_epoch: 1,
            owner: OwnerEpoch {
                owner: "owner".into(),
                epoch: 1,
            },
            execution_id: Uuid::new_v4().to_string(),
            operation_id: Uuid::new_v4().to_string(),
            view_id: Uuid::new_v4().to_string(),
        };
        let request = RemoteLaunchRequest {
            format_version: 1,
            request_id: Uuid::new_v4().to_string(),
            root: RootBinding {
                scope: source.scope.clone(),
                root_id: "fixture-root".into(),
                root,
            },
            observation: ObservationBinding {
                format_version: 1,
                attempt: AttemptContext {
                    execution_id: attempt.execution_id.clone(),
                    operation_id: attempt.operation_id.clone(),
                },
                view_id: attempt.view_id.clone(),
                owner: attempt.owner.clone(),
                namespace: Uuid::new_v4().to_string(),
                profile,
                exclusion_policy_digest: exclusion_policy_digest(&ImportPolicy::default()).unwrap(),
            },
            attempt,
            projection_policy: policy,
            accepted_source,
            accepted_seed: seed,
        };
        let store = LocalStore::open(temp.path().join("delta-store")).unwrap();
        Self {
            temp,
            source,
            authority,
            _cache: cache,
            registry,
            store,
            request,
            payload,
            file,
        }
    }
    fn session(&self) -> RemoteSession {
        self.registry
            .admit(
                &self.request.attempt,
                &self.request.root.root_id,
                &self.request.root.root,
                &SourceRequestControl::new(Instant::now() + Duration::from_secs(2)),
            )
            .unwrap()
    }
    fn manager_root(&self) -> PathBuf {
        self.temp.path().join("manager")
    }
    fn selection(&self) -> remote_launch::ValidatedRemoteSelection {
        remote_launch::validate_selection(self.session(), &self.request, RemoteOptions::default())
            .unwrap()
    }
    fn no_payload_reads(&self) {
        assert!(self
            .source
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|item| item != &self.payload
                && item != &self.file
                && item != &self.request.accepted_seed));
    }
    fn into_temp(self) -> tempfile::TempDir {
        // Consuming this fixture drops every original source/cache/registry/
        // authority owner before returning the private directory to the test.
        self.temp
    }
}
fn rejected<T>(result: Result<T>) -> anyhow::Error {
    match result {
        Ok(_) => panic!("selection unexpectedly accepted"),
        Err(error) => error,
    }
}
fn errno(error: &anyhow::Error) -> Option<i32> {
    error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
}
fn freeze_request(view: &VfsView) -> RemoteFreezeRequest {
    RemoteFreezeRequest {
        format_version: 1,
        intent_id: Uuid::new_v4().to_string(),
        capture: PartialCaptureRequest {
            nonce: Uuid::new_v4().to_string(),
            expected_begin: view.dirty_cursor().unwrap(),
            expected_head: view
                .partial_capture_head()
                .unwrap()
                .map(|receipt| receipt.request_nonce),
        },
    }
}

#[test]
fn finalizable_fork_is_explicit_initialized_and_cannot_reinterpret_original_open() {
    let fixture = Fixture::new(Shape::default());
    let workspace = fixture
        .selection()
        .open_finalizable(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let descriptor = workspace.view().descriptor().unwrap();
    assert_eq!(descriptor.remote_finalization_format, Some(1));
    assert_eq!(descriptor.partial_capture_format, Some(1));
    let request: serde_json::Value = serde_json::from_slice(
        &fs::read(
            workspace
                .manager()
                .root()
                .join("requests")
                .join(format!("{}.json", workspace.view().id())),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(request["format_version"], 5);
    assert_eq!(request["remote_binding"]["format_version"], 2);
    assert!(workspace.view().partial_capture_head().unwrap().is_none());
    drop(workspace);
    assert!(fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .is_err());
    let reopened = fixture
        .selection()
        .open_finalizable(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    assert_eq!(reopened.view().dirty_cursor().unwrap().generation, 0);
    fixture.no_payload_reads();
}

#[test]
fn frozen_view_excludes_effects_allows_immutable_reads_and_captures_only_live_data() {
    let fixture = Fixture::new(Shape {
        count: 1,
        ..Default::default()
    });
    let workspace = fixture
        .selection()
        .open_finalizable(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let view = workspace.view();
    let private = view
        .create(1, b"private", NodeKind::File, 0o600, b"")
        .unwrap();
    view.write(private.ino, 0, b"private frozen bytes", false)
        .unwrap();
    let request = freeze_request(view);
    let inode_count = view.descriptor().unwrap().inode_count;
    let mut frozen = view
        .freeze_remote(&request, Duration::from_secs(1))
        .unwrap();
    assert_eq!(frozen.intent().request, request);
    frozen
        .intent_reference()
        .verify(frozen.intent_bytes())
        .unwrap();
    for result in [
        view.write(private.ino, 0, b"changed", false).map(|_| ()),
        view.set_attr(private.ino, Some(0o644), Some(0), None, None)
            .map(|_| ()),
        view.create(1, b"later", NodeKind::File, 0o600, b"")
            .map(|_| ()),
        view.unlink(1, b"private", false),
        view.link(private.ino, 1, b"alias").map(|_| ()),
        view.rename(1, b"private", 1, b"renamed", 0),
        view.open_handle(private.ino, libc::O_RDWR, false)
            .map(|_| ()),
        view.open_handle(1, libc::O_RDONLY, true).map(|_| ()),
        view.read_directory_page(1, 99, 0, 4).map(|_| ()),
        view.release_partial_capture(&request.capture.nonce),
        view.release_observation(&request.capture.nonce),
    ] {
        assert_eq!(errno(&rejected(result)), Some(libc::EROFS));
    }
    fixture.no_payload_reads();
    // Inherited clean materialization changes representation, not the source
    // cursor, dirty entries, file bytes or final capture's selected roots.
    view.lookup(1, b"file-00000000").unwrap();
    assert!(view.descriptor().unwrap().inode_count > inode_count);
    assert_eq!(view.dirty_cursor().unwrap(), request.capture.expected_begin);
    assert_eq!(
        view.read(private.ino, 0, 20).unwrap(),
        b"private frozen bytes"
    );
    let receipt = frozen
        .capture(&PartialCaptureBudget::default(), &|| false)
        .unwrap();
    assert_eq!(
        receipt.consistency,
        rudder_engine::vfs::ObservationConsistency::LiveBestEffort
    );
    assert_eq!(receipt.begin, request.capture.expected_begin);
    assert_eq!(receipt.end, receipt.begin);
    assert_eq!(
        receipt,
        frozen
            .capture(&PartialCaptureBudget::default(), &|| false)
            .unwrap()
    );
    assert!(view
        .capture_partial_source(
            &ImportPolicy::default(),
            &request.capture,
            &PartialCaptureBudget::default()
        )
        .is_err());
    assert!(fixture.store.retention().unwrap().pins.is_empty());
    assert_eq!(view.dirty_cursor().unwrap(), request.capture.expected_begin);
    // Lookup and new directory-edge validation use the unchanged File's bounded
    // descriptor, never its 208 MiB content or private seed.
    let calls = fixture.source.calls.lock().unwrap();
    assert!(calls
        .iter()
        .all(|r| r != &fixture.payload && r != &fixture.request.accepted_seed));
    assert_eq!(calls.iter().filter(|r| *r == &fixture.file).count(), 1);
    assert!(fixture.file.size_bytes <= 64 * 1024);
}

#[test]
fn persisted_frozen_intent_never_reconstructs_guard_or_thaws_on_fresh_reopen() {
    let fixture = Fixture::new(Shape::default());
    let workspace = fixture
        .selection()
        .open_finalizable(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let request = freeze_request(workspace.view());
    let frozen = workspace
        .view()
        .freeze_remote(&request, Duration::from_secs(1))
        .unwrap();
    let bytes = frozen.intent_bytes().to_vec();
    let saved: rudder_engine::vfs::RemoteFreezeIntent = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(saved.request, request);
    drop(frozen);
    assert_eq!(
        errno(&rejected(
            workspace
                .view()
                .freeze_remote(&request, Duration::from_secs(1))
        )),
        Some(libc::EROFS)
    );
    drop(workspace);
    assert_eq!(
        errno(&rejected(fixture.selection().open_finalizable(
            fixture.manager_root(),
            fixture.store.clone()
        ))),
        Some(libc::EROFS)
    );
    let c = rusqlite::Connection::open(
        fixture
            .manager_root()
            .join("views")
            .join(&fixture.request.attempt.view_id)
            .join("namespace.sqlite"),
    )
    .unwrap();
    let stored: String = c
        .query_row(
            "SELECT body FROM remote_freeze_intent WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored.as_bytes(), bytes);
    assert!(fixture.store.retention().unwrap().pins.is_empty());
}

#[test]
fn frozen_owner_rejects_equal_selection_under_another_actual_manager_and_store() {
    let fixture = Fixture::new(Shape::default());
    let workspace = fixture
        .selection()
        .open_finalizable(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let other_store = LocalStore::open(fixture.temp.path().join("other-store")).unwrap();
    let other = fixture
        .selection()
        .open_finalizable(fixture.temp.path().join("other-manager"), other_store)
        .unwrap();
    assert_eq!(workspace.request(), other.request());
    assert_eq!(
        workspace.view().dirty_cursor().unwrap(),
        other.view().dirty_cursor().unwrap()
    );
    assert_eq!(
        workspace.view().remote_launch_digest().unwrap(),
        other.view().remote_launch_digest().unwrap()
    );
    let frozen = workspace
        .view()
        .freeze_remote(&freeze_request(workspace.view()), Duration::from_secs(1))
        .unwrap();
    frozen.check_for(workspace.view()).unwrap();
    frozen.check_for(&workspace.view().clone()).unwrap();
    assert!(frozen.check_for(other.view()).is_err());
    assert!(other
        .view()
        .descriptor()
        .unwrap()
        .remote_finalization_format
        .is_some());
    assert!(fixture.store.retention().unwrap().pins.is_empty());
}

#[test]
fn freeze_db_deadline_is_bounded_while_immutable_read_settles_and_later_writes_stay_closed() {
    let fixture = Fixture::new(Shape {
        count: 1,
        ..Default::default()
    });
    let workspace = fixture
        .selection()
        .open_finalizable(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let inherited = workspace.view().lookup(1, b"file-00000000").unwrap();
    let request = freeze_request(workspace.view());
    fixture.source.held_fetch.store(true, Ordering::Release);
    let view = workspace.view().clone();
    let read = std::thread::spawn(move || view.read(inherited.ino, 0, 4));
    let until = Instant::now() + Duration::from_secs(2);
    while !fixture.source.fetch_entered.load(Ordering::Acquire) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(fixture.source.fetch_entered.load(Ordering::Acquire));
    let began = Instant::now();
    assert_eq!(
        errno(&rejected(
            workspace
                .view()
                .freeze_remote(&request, Duration::from_millis(25))
        )),
        Some(libc::ETIMEDOUT)
    );
    assert!(began.elapsed() < Duration::from_secs(1));
    assert_eq!(
        errno(&rejected(workspace.view().write(
            inherited.ino,
            0,
            b"x",
            false
        ))),
        Some(libc::EROFS)
    );
    fixture.source.held_fetch.store(false, Ordering::Release);
    assert_eq!(read.join().unwrap().unwrap(), b"pppp");
    let frozen = workspace
        .view()
        .freeze_remote(&request, Duration::from_secs(1))
        .unwrap();
    assert_eq!(frozen.intent().request, request);
}

#[test]
fn external_sqlite_writer_cannot_exceed_freeze_deadline_and_same_owned_intent_can_retry() {
    let fixture = Fixture::new(Shape::default());
    let workspace = fixture
        .selection()
        .open_finalizable(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let request = freeze_request(workspace.view());
    let c = rusqlite::Connection::open(
        fixture
            .manager_root()
            .join("views")
            .join(workspace.view().id())
            .join("namespace.sqlite"),
    )
    .unwrap();
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let began = Instant::now();
    assert!(workspace
        .view()
        .freeze_remote(&request, Duration::from_millis(25))
        .is_err());
    assert!(began.elapsed() < Duration::from_secs(1));
    assert_eq!(
        errno(&rejected(workspace.view().create(
            1,
            b"late",
            NodeKind::File,
            0o600,
            b""
        ))),
        Some(libc::EROFS)
    );
    c.execute_batch("ROLLBACK").unwrap();
    let frozen = workspace
        .view()
        .freeze_remote(&request, Duration::from_secs(1))
        .unwrap();
    frozen.check_current().unwrap();
    assert_eq!(frozen.intent().request, request);
    assert!(fixture.store.retention().unwrap().pins.is_empty());
}

#[test]
fn open_registration_settles_before_freeze_and_cleanup_can_close_existing_handles() {
    let fixture = Fixture::new(Shape {
        nested_directory: true,
        ..Default::default()
    });
    let workspace = fixture
        .selection()
        .open_finalizable(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let root = workspace
        .view()
        .open_handle(1, libc::O_RDONLY, true)
        .unwrap();
    let rows = workspace
        .view()
        .read_directory_page(1, root, 0, 32)
        .unwrap();
    let nested = rows
        .entries
        .into_iter()
        .find(|entry| entry.name == b"nested")
        .unwrap()
        .ino;
    workspace.view().close_handle(1, root).unwrap();
    let request = freeze_request(workspace.view());
    fixture.source.held_fetch.store(true, Ordering::Release);
    let view = workspace.view().clone();
    let open = std::thread::spawn(move || view.open_handle(nested, libc::O_RDONLY, true));
    let until = Instant::now() + Duration::from_secs(2);
    while !fixture.source.fetch_entered.load(Ordering::Acquire) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(fixture.source.fetch_entered.load(Ordering::Acquire));
    assert_eq!(
        errno(&rejected(
            workspace
                .view()
                .freeze_remote(&request, Duration::from_millis(25))
        )),
        Some(libc::ETIMEDOUT)
    );
    fixture.source.held_fetch.store(false, Ordering::Release);
    let handle = open.join().unwrap().unwrap();
    assert!(workspace
        .view()
        .freeze_remote(&request, Duration::from_secs(1))
        .is_err());
    workspace.view().close_handle(nested, handle).unwrap();
    workspace
        .view()
        .freeze_remote(&request, Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        errno(&rejected(workspace.view().open_handle(
            nested,
            libc::O_RDONLY,
            true
        ))),
        Some(libc::EROFS)
    );
}

#[test]
fn changed_freeze_intent_or_view_custody_withholds_capture_without_releasing_data_ack() {
    let fixture = Fixture::new(Shape::default());
    let workspace = fixture
        .selection()
        .open_finalizable(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let private = workspace
        .view()
        .create(1, b"private", NodeKind::File, 0o600, b"")
        .unwrap();
    workspace
        .view()
        .write(private.ino, 0, b"retained after custody loss", false)
        .unwrap();
    let request = freeze_request(workspace.view());
    let mut frozen = workspace
        .view()
        .freeze_remote(&request, Duration::from_secs(1))
        .unwrap();
    let c = rusqlite::Connection::open(
        fixture
            .manager_root()
            .join("views")
            .join(workspace.view().id())
            .join("namespace.sqlite"),
    )
    .unwrap();
    let original = std::str::from_utf8(frozen.intent_bytes())
        .unwrap()
        .to_owned();
    let mut changed = frozen.intent().clone();
    changed.launch_digest = "f".repeat(64);
    c.execute(
        "UPDATE remote_freeze_intent SET body=? WHERE singleton=1",
        [String::from_utf8(canonical_json(&changed).unwrap()).unwrap()],
    )
    .unwrap();
    assert!(frozen
        .capture(&PartialCaptureBudget::default(), &|| false)
        .is_err());
    assert!(fixture
        .store
        .object_set_receipt(&format!(
            "vfs:partial:{}:{}",
            workspace.view().id(),
            request.capture.nonce
        ))
        .unwrap()
        .is_none());
    c.execute(
        "UPDATE remote_freeze_intent SET body=? WHERE singleton=1",
        [&original],
    )
    .unwrap();
    let receipt = frozen
        .capture(&PartialCaptureBudget::default(), &|| false)
        .unwrap();
    let before = fixture.store.retention().unwrap();
    let path = fixture
        .manager_root()
        .join("views")
        .join(workspace.view().id())
        .join("owner.lock");
    fs::rename(&path, path.with_file_name("replaced-owner.lock")).unwrap();
    fs::write(&path, b"replacement").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(frozen
        .capture(&PartialCaptureBudget::default(), &|| false)
        .is_err());
    assert_eq!(fixture.store.retention().unwrap(), before);
    rudder_engine::delta::read_partial_tree(&fixture.store, &receipt.retention).unwrap();
    assert!(fixture.store.retention().unwrap().pins.is_empty());
}

#[test]
fn strict_100k_remote_selection_reserves_namespace_without_hydrating_files_seed_or_complete_pins() {
    let fixture = Fixture::new(Shape {
        count: 100_101,
        ..Default::default()
    });
    let workspace = fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    assert_eq!(workspace.request(), &fixture.request);
    assert_eq!(
        workspace.view().dirty_cursor().unwrap().namespace,
        fixture.request.observation.namespace
    );
    assert_eq!(workspace.view().descriptor().unwrap().inode_count, 1);
    assert!(fixture.store.retention().unwrap().pins.is_empty());
    fixture.no_payload_reads();
    assert!(fixture.source.calls.lock().unwrap().len() <= 7);
    let view = workspace.view();
    assert_eq!(
        errno(&rejected(view.checkpoint_snapshot_unmounted())),
        Some(libc::EOPNOTSUPP)
    );
    assert_eq!(
        errno(&rejected(view.observe_source(
            &ImportPolicy::default(),
            &Uuid::new_v4().to_string(),
            &ObservationBudget::default()
        ))),
        Some(libc::EOPNOTSUPP)
    );
}

#[test]
fn actual_session_scope_root_and_attempt_mismatches_fail_before_metadata_or_namespace_effects() {
    let fixture = Fixture::new(Shape::default());
    let session = fixture.session();
    let mut cases = Vec::new();
    let mut value = fixture.request.clone();
    value.root.scope.provider_id = "other-provider".into();
    cases.push(value);
    let mut value = fixture.request.clone();
    value.root.scope.tenant_id = "other-tenant".into();
    cases.push(value);
    let mut value = fixture.request.clone();
    value.root.root_id = "other-root".into();
    cases.push(value);
    let mut value = fixture.request.clone();
    value.root.root = ObjectRef::from_bytes(ObjectKind::RunnableRoot, b"other");
    cases.push(value);
    let mut value = fixture.request.clone();
    value.attempt.project_id = "other-project".into();
    cases.push(value);
    let mut value = fixture.request.clone();
    value.attempt.run_id = "other-run".into();
    cases.push(value);
    let mut value = fixture.request.clone();
    value.attempt.run_epoch += 1;
    cases.push(value);
    let mut value = fixture.request.clone();
    value.attempt.owner.epoch += 1;
    value.observation.owner = value.attempt.owner.clone();
    cases.push(value);
    let mut value = fixture.request.clone();
    value.attempt.owner.owner = "other-owner".into();
    value.observation.owner = value.attempt.owner.clone();
    cases.push(value);
    let mut value = fixture.request.clone();
    value.attempt.execution_id = Uuid::new_v4().to_string();
    value.observation.attempt.execution_id = value.attempt.execution_id.clone();
    cases.push(value);
    let mut value = fixture.request.clone();
    value.attempt.operation_id = Uuid::new_v4().to_string();
    value.observation.attempt.operation_id = value.attempt.operation_id.clone();
    cases.push(value);
    let mut value = fixture.request.clone();
    value.attempt.view_id = Uuid::new_v4().to_string();
    value.observation.view_id = value.attempt.view_id.clone();
    cases.push(value);
    for value in cases {
        assert!(remote_launch::validate_selection(
            session.clone(),
            &value,
            RemoteOptions::default()
        )
        .is_err());
    }
    assert!(fixture.source.calls.lock().unwrap().is_empty());
    assert!(!fixture.manager_root().exists());
    let mut serialized = serde_json::to_value(&fixture.request).unwrap();
    serialized["live_permission"] = true.into();
    assert!(serde_json::from_value::<RemoteLaunchRequest>(serialized).is_err());
}

#[test]
fn generic_remote_resolution_is_not_strict_launch_validation() {
    let fixture = Fixture::new(Shape {
        different_directory: true,
        count: 1,
        ..Default::default()
    });
    let generic = VfsManager::open_remote(
        fixture.temp.path().join("generic"),
        fixture.store.clone(),
        fixture.session(),
        &fixture.request.root.root,
        RemoteOptions::default(),
    )
    .unwrap();
    drop(generic.fork_remote().unwrap());
    assert!(remote_launch::validate_selection(
        fixture.session(),
        &fixture.request,
        RemoteOptions::default()
    )
    .is_err());
    assert!(!fixture.manager_root().exists());
    fixture.no_payload_reads();
}

#[test]
fn original_remote_fork_keeps_v1_binding_and_original_request_bytes() {
    #[derive(Serialize)]
    struct OldBinding<'a> {
        format_version: u32,
        root: &'a RootBinding,
        attempt: &'a AttemptBinding,
    }
    let fixture = Fixture::new(Shape::default());
    let session = fixture.session();
    let old_binding = OldBinding {
        format_version: 1,
        root: session.binding(),
        attempt: session.attempt(),
    };
    let expected_binding_bytes = serde_json::to_string(&old_binding).unwrap();
    let expected_binding = serde_json::to_value(old_binding).unwrap();
    let manager = VfsManager::open_remote(
        fixture.manager_root(),
        fixture.store.clone(),
        session,
        &fixture.request.root.root,
        RemoteOptions::default(),
    )
    .unwrap();
    assert!(manager
        .fork_remote_with_namespace(&fixture.request.observation.namespace)
        .is_err());
    assert!(!manager
        .root()
        .join("requests")
        .join(format!("{}.json", fixture.request.attempt.view_id))
        .exists());
    let view = manager.fork_remote().unwrap();
    let request_bytes = fs::read(
        manager
            .root()
            .join("requests")
            .join(format!("{}.json", view.id())),
    )
    .unwrap();
    let expected = serde_json::json!({"format_version":3,"view_id":view.id(),"root":view.descriptor().unwrap().base,
        "topology":null,"owner":fixture.request.attempt.owner,"remote_binding":expected_binding});
    assert_eq!(request_bytes, canonical_json(&expected).unwrap());
    let connection = rusqlite::Connection::open(
        manager
            .root()
            .join("views")
            .join(view.id())
            .join("namespace.sqlite"),
    )
    .unwrap();
    let stored: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key='remote_binding'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, expected_binding_bytes);
    let capture = PartialCaptureRequest {
        nonce: Uuid::new_v4().to_string(),
        expected_begin: view.dirty_cursor().unwrap(),
        expected_head: None,
    };
    let mut no_work = PartialCaptureBudget::default();
    no_work.work.max_metadata_bytes = 0;
    assert!(view
        .capture_partial_source(&ImportPolicy::default(), &capture, &no_work)
        .is_err());
    let before = fixture.store.retention().unwrap();
    let before_state = view
        .inspect_partial_capture(&capture.nonce)
        .unwrap()
        .unwrap()
        .state;
    let mut with_null: serde_json::Value = serde_json::from_str(&stored).unwrap();
    with_null["launch"] = serde_json::Value::Null;
    connection
        .execute(
            "UPDATE meta SET value=? WHERE key='remote_binding'",
            [String::from_utf8(canonical_json(&with_null).unwrap()).unwrap()],
        )
        .unwrap();
    assert!(view.inspect_partial_capture(&capture.nonce).is_err());
    assert!(view.partial_capture_head().is_err());
    assert_eq!(fixture.store.retention().unwrap(), before);
    connection
        .execute(
            "UPDATE meta SET value=? WHERE key='remote_binding'",
            [&stored],
        )
        .unwrap();
    assert_eq!(
        view.inspect_partial_capture(&capture.nonce)
            .unwrap()
            .unwrap()
            .state,
        before_state
    );
    assert!(fixture.store.retention().unwrap().pins.is_empty());
}

#[test]
fn environment_partitions_topology_and_policy_encoding_are_explicitly_rejected() {
    for shape in [
        Shape {
            environment: true,
            ..Default::default()
        },
        Shape {
            partitions: true,
            ..Default::default()
        },
        Shape {
            hardlinks: true,
            ..Default::default()
        },
        Shape {
            noncanonical_policy: true,
            ..Default::default()
        },
    ] {
        let fixture = Fixture::new(shape);
        assert!(remote_launch::validate_selection(
            fixture.session(),
            &fixture.request,
            RemoteOptions::default()
        )
        .is_err());
        assert!(!fixture.manager_root().exists());
        fixture.no_payload_reads();
    }
    let fixture = Fixture::new(Shape::default());
    for field in ["accepted_source", "projection_policy"] {
        let mut value = fixture.request.clone();
        if field == "accepted_source" {
            value.accepted_source = ObjectRef::from_bytes(ObjectKind::SourceRoot, b"foreign");
        } else {
            value.projection_policy = ObjectRef::from_bytes(ObjectKind::Blob, b"{}");
        }
        assert!(remote_launch::validate_selection(
            fixture.session(),
            &value,
            RemoteOptions::default()
        )
        .is_err());
    }
    let mut value = fixture.request.clone();
    value.observation.profile = ObjectRef::from_bytes(ObjectKind::Blob, b"{}");
    assert!(
        remote_launch::validate_selection(fixture.session(), &value, RemoteOptions::default())
            .is_err()
    );
}

#[test]
fn fresh_session_reopen_preserves_namespace_dirty_generation_and_full_immutable_selection() {
    let fixture = Fixture::new(Shape {
        count: 1,
        ..Default::default()
    });
    let workspace = fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let created = workspace
        .view()
        .create(1, b"private", NodeKind::File, 0o600, b"")
        .unwrap();
    workspace
        .view()
        .write(created.ino, 0, b"retained dirty bytes", false)
        .unwrap();
    let cursor = workspace.view().dirty_cursor().unwrap();
    let (manager, view, request) = workspace.into_parts();
    assert_eq!(request, fixture.request);
    drop(view);
    drop(manager);
    let restored: RemoteLaunchRequest =
        serde_json::from_slice(&canonical_json(&fixture.request).unwrap()).unwrap();
    let reopened =
        remote_launch::validate_selection(fixture.session(), &restored, RemoteOptions::default())
            .unwrap()
            .open(fixture.manager_root(), fixture.store.clone())
            .unwrap();
    assert_eq!(reopened.view().dirty_cursor().unwrap(), cursor);
    assert_eq!(
        reopened.view().read(created.ino, 0, 20).unwrap(),
        b"retained dirty bytes"
    );
    drop(reopened);
    fixture
        .source
        .root_unavailable
        .store(true, Ordering::Release);
    assert!(fixture
        .registry
        .saved_receipt(&restored.root.root_id)
        .unwrap()
        .is_some());
    assert!(
        fixture
            .registry
            .admit(
                &restored.attempt,
                &restored.root.root_id,
                &restored.root.root,
                &SourceRequestControl::new(Instant::now() + Duration::from_secs(2))
            )
            .is_err(),
        "saved request/cache/receipt cannot replace fresh admission"
    );
    fixture
        .source
        .root_unavailable
        .store(false, Ordering::Release);
    for change in ["namespace", "request", "seed"] {
        let mut request = fixture.request.clone();
        match change {
            "namespace" => request.observation.namespace = Uuid::new_v4().to_string(),
            "request" => request.request_id = Uuid::new_v4().to_string(),
            _ => {
                request.accepted_seed =
                    ObjectRef::from_bytes(ObjectKind::Directory, b"other-seed-expected-only")
            }
        }
        let selection = remote_launch::validate_selection(
            fixture.session(),
            &request,
            RemoteOptions::default(),
        )
        .unwrap();
        assert!(selection
            .open(fixture.manager_root(), fixture.store.clone())
            .is_err());
    }
    assert_eq!(
        fixture
            .selection()
            .open(fixture.manager_root(), fixture.store.clone())
            .unwrap()
            .view()
            .dirty_cursor()
            .unwrap(),
        cursor
    );
    assert!(fixture.source.admissions.load(Ordering::Acquire) >= 6);
    fixture.no_payload_reads();
}

#[test]
fn into_parts_retains_actual_source_cache_registry_and_authority_without_original_owners() {
    let fixture = Fixture::new(Shape {
        count: 1,
        ..Default::default()
    });
    let source = Arc::downgrade(&fixture.source);
    let authority = Arc::downgrade(&fixture.authority);
    let workspace = fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let (manager, view, request) = workspace.into_parts();
    let temp = fixture.into_temp();
    // The view owns its manager, so dropping this independent manager handle
    // also must not retire the actual session or its source/authority owners.
    drop(manager);
    assert!(source.upgrade().is_some());
    let inherited = view.lookup(1, b"file-00000000").unwrap();
    assert_eq!(view.read(inherited.ino, 0, 4).unwrap(), b"pppp");
    authority
        .upgrade()
        .unwrap()
        .0
        .store(true, Ordering::Release);
    assert!(view.lookup(1, b"file-00000000").is_err());
    drop(view);
    let until = Instant::now() + Duration::from_secs(2);
    while source.strong_count() != 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(source.upgrade().is_none());
    assert!(authority.upgrade().is_none());
    // Serialized expected identities outlive all resources and retain no live
    // authority; the only supported reopen still requires fresh admission.
    request.validate().unwrap();
    drop(temp);
}

#[test]
fn timed_out_fetch_keeps_owned_source_until_transport_actually_settles() {
    let fixture = Fixture::new(Shape {
        count: 1,
        ..Default::default()
    });
    let source = Arc::downgrade(&fixture.source);
    let held = fixture.source.held_fetch.clone();
    let entered = fixture.source.fetch_entered.clone();
    let workspace = remote_launch::validate_selection(
        fixture.session(),
        &fixture.request,
        RemoteOptions {
            request_timeout: Duration::from_millis(500),
            ..Default::default()
        },
    )
    .unwrap()
    .open(fixture.manager_root(), fixture.store.clone())
    .unwrap();
    let inherited = workspace.view().lookup(1, b"file-00000000").unwrap();
    held.store(true, Ordering::Release);
    let view = workspace.view().clone();
    let fetch = std::thread::spawn(move || view.read(inherited.ino, 0, 4));
    let until = Instant::now() + Duration::from_secs(2);
    while !entered.load(Ordering::Acquire) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(entered.load(Ordering::Acquire));
    let result = fetch.join().unwrap();
    assert_eq!(errno(&rejected(result)), Some(libc::ETIMEDOUT));
    drop(workspace);
    let temp = fixture.into_temp();
    // No request, view, manager, registry or cache owner remains. The running
    // worker still owns source/admission even though its caller timed out.
    assert!(source.upgrade().is_some());
    held.store(false, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(2);
    while source.strong_count() != 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(source.upgrade().is_none());
    drop(temp);
}

#[test]
fn persisted_reserved_namespace_corruption_is_never_rebound_or_repaired() {
    let fixture = Fixture::new(Shape::default());
    let workspace = fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let database = workspace
        .manager()
        .root()
        .join("views")
        .join(workspace.view().id())
        .join("namespace.sqlite");
    drop(workspace);
    let connection = rusqlite::Connection::open(&database).unwrap();
    let changed = Uuid::new_v4().to_string();
    connection
        .execute("UPDATE observation_state SET namespace=?", [&changed])
        .unwrap();
    drop(connection);
    assert!(fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .is_err());
    let connection = rusqlite::Connection::open(&database).unwrap();
    let actual: String = connection
        .query_row("SELECT namespace FROM observation_state", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(actual, changed);
    assert!(fixture.store.retention().unwrap().pins.is_empty());
}

#[test]
fn oversized_saved_fork_request_is_bounded_and_never_repaired() {
    let fixture = Fixture::new(Shape::default());
    let workspace = fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let reservation = workspace
        .manager()
        .root()
        .join("requests")
        .join(format!("{}.json", workspace.view().id()));
    drop(workspace);
    fs::write(&reservation, vec![b' '; 65537]).unwrap();
    assert_eq!(
        errno(&rejected(
            fixture
                .selection()
                .open(fixture.manager_root(), fixture.store.clone())
        )),
        Some(libc::EFBIG)
    );
    assert_eq!(fs::metadata(&reservation).unwrap().len(), 65537);
    assert!(fixture.store.retention().unwrap().pins.is_empty());
}

#[test]
fn legacy_binding_parser_and_generic_v1_reopen_reject_new_binding_before_view_effects() {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OldBinding {
        #[allow(dead_code)]
        format_version: u32,
        #[allow(dead_code)]
        root: RootBinding,
        #[allow(dead_code)]
        attempt: AttemptBinding,
    }
    let fixture = Fixture::new(Shape::default());
    let workspace = fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let cursor = workspace.view().dirty_cursor().unwrap();
    let database = workspace
        .manager()
        .root()
        .join("views")
        .join(workspace.view().id())
        .join("namespace.sqlite");
    let connection = rusqlite::Connection::open(&database).unwrap();
    let binding: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key='remote_binding'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(serde_json::from_str::<OldBinding>(&binding).is_err());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&binding).unwrap()["format_version"],
        2
    );
    drop(connection);
    drop(workspace);
    let before = fs::read(&database).unwrap();
    let old = VfsManager::open_remote(
        fixture.manager_root(),
        fixture.store.clone(),
        fixture.session(),
        &fixture.request.root.root,
        RemoteOptions::default(),
    )
    .unwrap();
    assert!(old.open_remote_view().is_err());
    assert!(old.fork_remote().is_err());
    drop(old);
    assert_eq!(fs::read(&database).unwrap(), before);
    let reopened = fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    assert_eq!(reopened.view().dirty_cursor().unwrap(), cursor);
}

#[test]
fn metadata_count_bytes_total_deadline_and_revocation_do_not_hydrate_or_publish_views() {
    for options in [
        RemoteOptions {
            max_initial_objects: 1,
            ..Default::default()
        },
        RemoteOptions {
            max_initial_bytes: 1,
            ..Default::default()
        },
    ] {
        let fixture = Fixture::new(Shape::default());
        assert_eq!(
            errno(&rejected(remote_launch::validate_selection(
                fixture.session(),
                &fixture.request,
                options
            ))),
            Some(libc::EAGAIN)
        );
        assert!(!fixture.manager_root().exists());
        fixture.no_payload_reads();
    }
    let fixture = Fixture::new(Shape::default());
    fixture.source.delay_ms.store(15, Ordering::Release);
    let start = Instant::now();
    assert_eq!(
        errno(&rejected(remote_launch::validate_selection(
            fixture.session(),
            &fixture.request,
            RemoteOptions {
                request_timeout: Duration::from_millis(45),
                ..Default::default()
            }
        ))),
        Some(libc::ETIMEDOUT)
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(!fixture.manager_root().exists());
    let fixture = Fixture::new(Shape::default());
    fixture.source.revoke_after.store(2, Ordering::Release);
    assert!(remote_launch::validate_selection(
        fixture.session(),
        &fixture.request,
        RemoteOptions::default()
    )
    .is_err());
    assert!(!fixture.manager_root().exists());
    fixture.no_payload_reads();
    let fixture = Fixture::new(Shape::default());
    let selection = fixture.selection();
    fixture.authority.0.store(true, Ordering::Release);
    assert!(selection
        .open(fixture.manager_root(), fixture.store.clone())
        .is_err());
    assert!(!fixture.manager_root().exists());
}

#[test]
fn namespace_four_rejects_corrupt_saved_launch_bindings_on_already_open_views_without_ack() {
    let fixture = Fixture::new(Shape::default());
    let workspace = fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let request = PartialCaptureRequest {
        nonce: Uuid::new_v4().to_string(),
        expected_begin: workspace.view().dirty_cursor().unwrap(),
        expected_head: None,
    };
    let mut no_work = PartialCaptureBudget::default();
    no_work.work.max_metadata_bytes = 0;
    assert!(workspace
        .view()
        .capture_partial_source(&ImportPolicy::default(), &request, &no_work)
        .is_err());
    let connection = rusqlite::Connection::open(
        workspace
            .manager()
            .root()
            .join("views")
            .join(workspace.view().id())
            .join("namespace.sqlite"),
    )
    .unwrap();
    let original: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key='remote_binding'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let before = fixture.store.retention().unwrap();
    for field in [
        "version",
        "v1_with_launch",
        "missing_launch",
        "unknown_field",
        "root",
        "attempt",
        "namespace",
        "profile",
        "exclusion",
        "source",
        "seed",
        "projection_policy",
        "request_id",
        "noncanonical_bytes",
    ] {
        let mut changed: serde_json::Value = serde_json::from_str(&original).unwrap();
        match field {
            "version" => changed["format_version"] = 99.into(),
            "v1_with_launch" => changed["format_version"] = 1.into(),
            "missing_launch" => {
                changed.as_object_mut().unwrap().remove("launch");
            }
            "unknown_field" => changed["launch"]["live_permission"] = true.into(),
            "root" => changed["root"]["root_id"] = "different-root".into(),
            "attempt" => changed["attempt"]["run_epoch"] = 2.into(),
            "namespace" => {
                changed["launch"]["observation"]["namespace"] = Uuid::new_v4().to_string().into()
            }
            "profile" => {
                changed["launch"]["observation"]["profile"] = serde_json::to_value(
                    ObjectRef::from_bytes(ObjectKind::Blob, b"different-profile"),
                )
                .unwrap()
            }
            "exclusion" => {
                changed["launch"]["observation"]["exclusion_policy_digest"] = "f".repeat(64).into()
            }
            "source" => {
                changed["launch"]["accepted_source"] = serde_json::to_value(ObjectRef::from_bytes(
                    ObjectKind::SourceRoot,
                    b"different-source",
                ))
                .unwrap()
            }
            "seed" => {
                changed["launch"]["accepted_seed"] = serde_json::to_value(ObjectRef::from_bytes(
                    ObjectKind::Directory,
                    b"different-seed-expected-only",
                ))
                .unwrap()
            }
            "projection_policy" => {
                changed["launch"]["projection_policy"] = serde_json::to_value(
                    ObjectRef::from_bytes(ObjectKind::Blob, b"different-policy"),
                )
                .unwrap()
            }
            "noncanonical_bytes" => {}
            _ => changed["launch"]["request_id"] = Uuid::new_v4().to_string().into(),
        }
        let mut changed = if field == "noncanonical_bytes" {
            original.clone()
        } else {
            String::from_utf8(canonical_json(&changed).unwrap()).unwrap()
        };
        if field == "noncanonical_bytes" {
            changed.insert(0, ' ');
        }
        connection
            .execute(
                "UPDATE meta SET value=? WHERE key='remote_binding'",
                [&changed],
            )
            .unwrap();
        assert!(
            workspace
                .view()
                .inspect_partial_capture(&request.nonce)
                .is_err(),
            "accepted corrupt {field}"
        );
        assert!(workspace.view().partial_capture_head().is_err());
        assert!(workspace
            .view()
            .capture_partial_source(
                &ImportPolicy::default(),
                &request,
                &PartialCaptureBudget::default(),
            )
            .is_err());
        assert_eq!(fixture.store.retention().unwrap(), before);
        assert!(fixture
            .store
            .object_set_receipt(&format!(
                "vfs:partial:{}:{}",
                workspace.view().id(),
                request.nonce
            ))
            .unwrap()
            .is_none());
    }
    connection
        .execute(
            "UPDATE meta SET value=? WHERE key='remote_binding'",
            [&original],
        )
        .unwrap();
    assert!(workspace
        .view()
        .inspect_partial_capture(&request.nonce)
        .unwrap()
        .is_some());
    assert!(workspace.view().partial_capture_head().unwrap().is_none());
    fixture.no_payload_reads();
}

#[test]
fn activated_namespace_four_reopens_without_rebinding_and_complete_consumers_stay_unsupported() {
    let fixture = Fixture::new(Shape::default());
    let workspace = fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    let begin = workspace.view().dirty_cursor().unwrap();
    let mut budget = PartialCaptureBudget::default();
    budget.work.max_metadata_bytes = 0;
    assert!(workspace
        .view()
        .capture_partial_source(
            &ImportPolicy::default(),
            &PartialCaptureRequest {
                nonce: Uuid::new_v4().to_string(),
                expected_begin: begin.clone(),
                expected_head: None
            },
            &budget
        )
        .is_err());
    assert_eq!(
        workspace
            .view()
            .descriptor()
            .unwrap()
            .partial_capture_format,
        Some(1)
    );
    drop(workspace);
    let reopened = fixture
        .selection()
        .open(fixture.manager_root(), fixture.store.clone())
        .unwrap();
    assert_eq!(reopened.view().dirty_cursor().unwrap(), begin);
    assert_eq!(
        reopened.view().descriptor().unwrap().partial_capture_format,
        Some(1)
    );
    assert_eq!(
        errno(&rejected(reopened.view().checkpoint_snapshot_unmounted())),
        Some(libc::EOPNOTSUPP)
    );
    assert!(fixture.store.retention().unwrap().pins.is_empty());
    fixture.no_payload_reads();
}
