//! Synthetic retained audit DATA, not a process-drain or current-grant fixture.
use rudder_engine::{
    delta::{DeltaPlan, PartialTreeReceipt},
    execution::{QuiescedPartialCheckpointV1, StopAndCheckpoint},
    partial_checkpoint::{
        self, DrainAuditDataV1, NamespaceAuditIdentity, NetworkAuditDataV1,
        PartialCheckpointDataIntent, PartialCheckpointDataReceipt, PartialCheckpointPublicationV1,
        PartialCheckpointTemplateV1, PreparedPartialCheckpointData,
    },
    paths::FabricPaths,
    protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef, OwnerEpoch},
    remote_cache::CacheScope,
    remote_root::{AttemptBinding, RemoteObjectPath, RootBinding},
    service::TransferSession,
    snapshot::{RunnableRoot, SourceV2},
    store::{DirectoryEntry, DirectoryManifest, EntryNode, ImportPolicy, LocalStore},
    vfs::{
        exclusion_policy_digest,
        partial_rpc::{PartialBinding, PROTOCOL},
        rpc::{AttemptContext, ObservationBinding},
        DirtyCursor, HardlinkTopology, ObservationConsistency, PartialCaptureReceipt,
        PartialCaptureRequest, RemoteFreezeIntent, RemoteFreezeRequest,
    },
    workspace::WorkspaceProfile,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{cell::RefCell, collections::BTreeMap, fs, time::Duration};
use uuid::Uuid;

const ABOVE_JS: u64 = 9_007_199_254_740_993;
fn metadata(kind: ObjectKind, value: &impl Serialize) -> (ObjectRef, Vec<u8>) {
    let bytes = canonical_json(value).unwrap();
    (ObjectRef::from_bytes(kind, &bytes), bytes)
}
fn blob(bytes: &[u8]) -> (ObjectRef, Vec<u8>) {
    (ObjectRef::from_bytes(ObjectKind::Blob, bytes), bytes.into())
}
fn directory(files: &[(&[u8], ObjectRef)]) -> (ObjectRef, Vec<u8>) {
    let mut entries = files
        .iter()
        .map(|(name, object)| {
            DirectoryEntry::new(
                name,
                EntryNode::File {
                    object: object.clone(),
                    mode: 0o644,
                },
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.name_hex.cmp(&b.name_hex));
    metadata(
        ObjectKind::Directory,
        &DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries,
        },
    )
}

fn paged_directory(
    files: &[(&[u8], ObjectRef, u64)],
) -> ((ObjectRef, Vec<u8>), Vec<(ObjectRef, Vec<u8>)>) {
    use rudder_engine::directory::{DirectoryStore, PagedDirectory};
    #[derive(Default)]
    struct Bytes(RefCell<BTreeMap<ObjectRef, Vec<u8>>>);
    impl DirectoryStore for Bytes {
        fn read(&self, reference: &ObjectRef) -> anyhow::Result<Vec<u8>> {
            self.0
                .borrow()
                .get(reference)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("fixture metadata absent"))
        }
        fn put(&self, kind: ObjectKind, bytes: &[u8]) -> anyhow::Result<ObjectRef> {
            rudder_engine::directory::references_from_bytes(kind, bytes)?;
            let reference = ObjectRef::from_bytes(kind, bytes);
            self.0.borrow_mut().insert(reference.clone(), bytes.into());
            Ok(reference)
        }
    }
    let objects = Bytes::default();
    let entries = files.iter().map(|(name, reference, size)| {
        rudder_engine::directory::DirectoryEntry::new(
            name,
            rudder_engine::directory::EntryNode::File {
                object: reference.clone(),
                logical_size: *size,
                mode: 0o644,
            },
        )
        .unwrap()
    });
    let root = PagedDirectory::new(&objects).build(0o755, entries).unwrap();
    let mut bytes = objects.0.into_inner();
    let root_bytes = bytes.remove(&root).unwrap();
    ((root, root_bytes), bytes.into_iter().collect())
}
struct Fixture {
    _root: tempfile::TempDir,
    paths: FabricPaths,
    store: LocalStore,
    template: PartialCheckpointTemplateV1,
    q: QuiescedPartialCheckpointV1,
    prepared: PreparedPartialCheckpointData,
    receipt: PartialCheckpointDataReceipt,
    base_objects: Vec<(ObjectRef, Vec<u8>)>,
    inherited: ObjectRef,
    inherited_descriptor: Option<ObjectRef>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_paged(false)
    }
    fn with_paged(paged: bool) -> Self {
        let root = tempfile::Builder::new()
            .prefix("partial-checkpoint-")
            .tempdir()
            .unwrap();
        let paths = FabricPaths::new(root.path());
        let store = LocalStore::open(&paths.cas).unwrap();
        let initialization = store.begin_staging(Duration::from_secs(60)).unwrap();
        let initial_blob = store.put_blob(b"ledger initialization").unwrap();
        let initial = store
            .retain_object_set(&initialization.id, "initial", &[initial_blob])
            .unwrap();
        store.release_object_set(&initial).unwrap();
        let db = rusqlite::Connection::open(paths.cas.join("object-sets.sqlite")).unwrap();
        db.execute(
            "UPDATE object_set_meta SET next_generation=? WHERE id=1",
            [ABOVE_JS as i64],
        )
        .unwrap();
        drop(db);
        let old = blob(b"unchanged inherited content");
        let mut extra_base = Vec::new();
        let inherited = if paged {
            let node = rudder_engine::content::FileNode {
                format_version: 1,
                layout: rudder_engine::content::FILE_LAYOUT.into(),
                level: 0,
                logical_size: old.0.size_bytes,
                children: vec![old.0.clone()],
            };
            node.validate().unwrap();
            let object = metadata(ObjectKind::File, &node);
            extra_base.push(object.clone());
            object.0
        } else {
            old.0.clone()
        };
        let old_name: &[u8] = if paged { b"\x80-old" } else { b"old" };
        let before = if paged {
            let (root, mut shards) =
                paged_directory(&[(old_name, inherited.clone(), old.0.size_bytes)]);
            extra_base.append(&mut shards);
            root
        } else {
            directory(&[(old_name, inherited.clone())])
        };
        let profile = WorkspaceProfile::source_only();
        let profile_object = metadata(ObjectKind::Blob, &profile);
        let policy = metadata(
            ObjectKind::Blob,
            &serde_json::json!({"format_version":1,"excluded_paths_hex":[]}),
        );
        let topology = metadata(
            ObjectKind::Blob,
            &HardlinkTopology {
                format_version: 1,
                groups: vec![],
            },
        );
        let base_source = SourceV2 {
            format_version: 2,
            consistency: CheckpointConsistency::Quiesced,
            directory: before.0.clone(),
            projection_policy: policy.0.clone(),
        };
        let source_object = metadata(ObjectKind::SourceRoot, &base_source);
        let base_runnable = RunnableRoot {
            format_version: 1,
            consistency: CheckpointConsistency::Quiesced,
            source: source_object.0.clone(),
            environment: None,
            view_root: before.0.clone(),
            profile: profile_object.0.clone(),
            hardlinks: topology.0.clone(),
            knowledge_cursor: ABOVE_JS + 6,
        };
        let root_object = metadata(ObjectKind::RunnableRoot, &base_runnable);
        let owner = OwnerEpoch {
            owner: "fixture-owner".into(),
            epoch: ABOVE_JS,
        };
        let attempt = AttemptBinding {
            project_id: "fixture-project".into(),
            run_id: "fixture-run".into(),
            run_epoch: ABOVE_JS + 2,
            owner: owner.clone(),
            execution_id: "11111111-1111-1111-1111-111111111111".into(),
            operation_id: "22222222-2222-2222-2222-222222222222".into(),
            view_id: "33333333-3333-3333-3333-333333333333".into(),
        };
        let observation = ObservationBinding {
            format_version: 1,
            attempt: AttemptContext {
                execution_id: attempt.execution_id.clone(),
                operation_id: attempt.operation_id.clone(),
            },
            view_id: attempt.view_id.clone(),
            owner: owner.clone(),
            namespace: "55555555-5555-5555-5555-555555555555".into(),
            profile: profile_object.0.clone(),
            exclusion_policy_digest: exclusion_policy_digest(&ImportPolicy::default()).unwrap(),
        };
        let binding = RootBinding {
            scope: CacheScope {
                provider_id: "fixture-provider".into(),
                tenant_id: "fixture-tenant".into(),
            },
            root_id: "base-fixture".into(),
            root: root_object.0.clone(),
        };
        let template = PartialCheckpointTemplateV1 {
            format_version: 1,
            selection: rudder_engine::vfs::remote_launch::RemoteLaunchRequest {
                format_version: 1,
                request_id: "44444444-4444-4444-4444-444444444444".into(),
                root: binding.clone(),
                attempt: attempt.clone(),
                observation: observation.clone(),
                projection_policy: policy.0.clone(),
                accepted_source: source_object.0.clone(),
                // Intentionally unavailable. This is a prior identity, not seed bytes or authority.
                accepted_seed: ObjectRef::from_bytes(
                    ObjectKind::Directory,
                    b"private seed is never hydrated",
                ),
            },
            base_runnable,
            base_source,
            profile,
        };
        let stop = StopAndCheckpoint {
            request_id: "66666666-6666-6666-6666-666666666666".into(),
            execution_id: attempt.execution_id.clone(),
            operation_id: attempt.operation_id.clone(),
            view_id: attempt.view_id.clone(),
            owner: owner.clone(),
            run_id: attempt.run_id.clone(),
            run_epoch: attempt.run_epoch,
        };
        let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let changed = blob(b"new producer bytes");
        let (after, after_shards) = if paged {
            paged_directory(&[
                (b"changed", changed.0.clone(), changed.0.size_bytes),
                (old_name, inherited.clone(), old.0.size_bytes),
            ])
        } else {
            (
                directory(&[
                    (b"changed", changed.0.clone()),
                    (old_name, inherited.clone()),
                ]),
                Vec::new(),
            )
        };
        for (reference, bytes) in [&changed, &after].into_iter().chain(after_shards.iter()) {
            store.receive_staged(&stage.id, reference, bytes).unwrap();
        }
        let mut produced = vec![changed.0.clone(), after.0.clone()];
        produced.extend(after_shards.iter().map(|(r, _)| r.clone()));
        produced.sort();
        let mut boundaries = vec![
            RemoteObjectPath {
                reference: inherited.clone(),
                edges: if paged { vec![1, 0, 0] } else { vec![1, 0] },
            },
            RemoteObjectPath {
                reference: topology.0.clone(),
                edges: vec![3],
            },
        ];
        boundaries.sort_by(|a, b| a.reference.cmp(&b.reference));
        let plan = DeltaPlan {
            format_version: 1,
            capture_id: stop.request_id.clone(),
            base: binding.clone(),
            attempt: attempt.clone(),
            directory: after.0.clone(),
            hardlinks: topology.0.clone(),
            local_objects: produced.clone(),
            remote_boundaries: boundaries,
        };
        let plan_object = metadata(ObjectKind::Blob, &plan);
        store
            .receive_staged(&stage.id, &plan_object.0, &plan_object.1)
            .unwrap();
        produced.push(plan_object.0.clone());
        let capture_retention = store
            .retain_object_set(&stage.id, "fixture:capture", &produced)
            .unwrap();
        let cursor = DirtyCursor {
            view_id: attempt.view_id.clone(),
            owner: owner.clone(),
            namespace: observation.namespace.clone(),
            generation: ABOVE_JS + 4,
        };
        let launch_digest =
            hex::encode(Sha256::digest(canonical_json(&template.selection).unwrap()));
        let freeze = RemoteFreezeIntent {
            format_version: 1,
            launch_digest: launch_digest.clone(),
            request: RemoteFreezeRequest {
                format_version: 1,
                intent_id: "77777777-7777-7777-7777-777777777777".into(),
                capture: PartialCaptureRequest {
                    nonce: stop.request_id.clone(),
                    expected_begin: cursor.clone(),
                    expected_head: None,
                },
            },
        };
        let partial = PartialBinding {
            protocol: PROTOCOL.into(),
            format_version: 1,
            launch_request_digest: launch_digest.clone(),
            root: binding.clone(),
            attempt: attempt.clone(),
            observation: observation.clone(),
        };
        let drain = DrainAuditDataV1 {
            format_version: 1,
            execution_id: attempt.execution_id.clone(),
            operation_id: attempt.operation_id.clone(),
            view_id: attempt.view_id.clone(),
            owner: owner.clone(),
            execution_request_digest: "a".repeat(64),
            launch_request_digest: launch_digest,
            agent_exit: 143,
            stop: stop.clone(),
            partial_binding_digest: partial.digest().unwrap(),
            endpoint_instance: "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee".into(),
            network: NetworkAuditDataV1 {
                format_version: 1,
                profile_digest: "b".repeat(64),
                network_id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into(),
                readiness_nonce: "cccccccc-cccc-cccc-cccc-cccccccccccc".into(),
                mode: "offline_v1".into(),
                user: NamespaceAuditIdentity {
                    device: ABOVE_JS + 10,
                    inode: u64::MAX - 3,
                },
                network: NamespaceAuditIdentity {
                    device: ABOVE_JS + 12,
                    inode: u64::MAX - 1,
                },
            },
            kernel_drained: true,
        };
        let freeze_object = metadata(ObjectKind::Blob, &freeze);
        let drain_object = metadata(ObjectKind::Blob, &drain);
        let evidence_stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        for (reference, bytes) in [&freeze_object, &drain_object] {
            store
                .receive_staged(&evidence_stage.id, reference, bytes)
                .unwrap();
        }
        let evidence = store
            .retain_object_set(
                &evidence_stage.id,
                "fixture:evidence",
                &[freeze_object.0.clone(), drain_object.0.clone()],
            )
            .unwrap();
        let q = QuiescedPartialCheckpointV1 {
            format_version: 1,
            capture: PartialCaptureReceipt {
                format_version: 1,
                view_id: attempt.view_id.clone(),
                owner,
                attempt,
                base: binding,
                begin: cursor.clone(),
                end: cursor,
                exclusion_policy_digest: observation.exclusion_policy_digest.clone(),
                request_nonce: stop.request_id.clone(),
                previous_head: None,
                head_sequence: 1,
                retention: PartialTreeReceipt {
                    format_version: 1,
                    plan: plan_object.0,
                    local: capture_retention,
                },
                consistency: ObservationConsistency::LiveBestEffort,
            },
            stop,
            observation,
            execution_request_digest: "a".repeat(64),
            freeze_intent: freeze_object.0,
            drain_record: drain_object.0,
            evidence_retention: evidence,
        };
        let wrapper_stage = store.begin_staging(Duration::from_secs(60)).unwrap();
        let prepared = partial_checkpoint::prepare_partial_checkpoint_data(
            &store,
            &wrapper_stage.id,
            &q,
            &template,
        )
        .unwrap();
        let receipt = prepared.retain(&store).unwrap();
        Self {
            _root: root,
            paths,
            store,
            template,
            q,
            prepared,
            receipt,
            base_objects: vec![
                old.clone(),
                before,
                profile_object,
                policy,
                topology,
                source_object,
                root_object,
            ]
            .into_iter()
            .chain(extra_base)
            .collect(),
            inherited: old.0,
            inherited_descriptor: paged.then_some(inherited),
        }
    }

    fn prepare(&self, q: &QuiescedPartialCheckpointV1) -> anyhow::Result<()> {
        let stage = self.store.begin_staging(Duration::from_secs(60))?;
        let result = partial_checkpoint::prepare_partial_checkpoint_data(
            &self.store,
            &stage.id,
            q,
            &self.template,
        );
        self.store.abandon_staging(&stage.id)?;
        result.map(|_| ())
    }

    /// Retain newly sealed audit data so semantic failures cannot be masked by
    /// old SHA/ref/receipt mismatches. This never supplies process authority.
    fn with_evidence(
        &self,
        freeze: &serde_json::Value,
        drain: &serde_json::Value,
    ) -> QuiescedPartialCheckpointV1 {
        let stage = self.store.begin_staging(Duration::from_secs(60)).unwrap();
        let freeze = metadata(ObjectKind::Blob, freeze);
        let drain = metadata(ObjectKind::Blob, drain);
        for (reference, bytes) in [&freeze, &drain] {
            self.store
                .receive_staged(&stage.id, reference, bytes)
                .unwrap();
        }
        let evidence = self
            .store
            .retain_object_set(
                &stage.id,
                &format!("fixture:evidence:{}", Uuid::new_v4()),
                &[freeze.0.clone(), drain.0.clone()],
            )
            .unwrap();
        let mut q = self.q.clone();
        q.freeze_intent = freeze.0;
        q.drain_record = drain.0;
        q.evidence_retention = evidence;
        q
    }

    fn with_plan(&self, plan: &DeltaPlan) -> QuiescedPartialCheckpointV1 {
        let stage = self.store.begin_staging(Duration::from_secs(60)).unwrap();
        let (reference, bytes) = metadata(ObjectKind::Blob, plan);
        self.store
            .receive_staged(&stage.id, &reference, &bytes)
            .unwrap();
        let mut objects = plan.local_objects.clone();
        objects.push(reference.clone());
        let retained = self
            .store
            .retain_object_set(
                &stage.id,
                &format!("fixture:plan:{}", Uuid::new_v4()),
                &objects,
            )
            .unwrap();
        let mut q = self.q.clone();
        q.capture.retention = PartialTreeReceipt {
            format_version: 1,
            plan: reference,
            local: retained,
        };
        q
    }

    fn with_publication(
        &self,
        mut value: PartialCheckpointPublicationV1,
    ) -> PartialCheckpointDataReceipt {
        value.payload.publication_id = value.payload.expected_publication_id().unwrap();
        value.references = value.payload.references().unwrap();
        let (reference, bytes) = metadata(ObjectKind::ActionResult, &value);
        let stage = self.store.begin_staging(Duration::from_secs(60)).unwrap();
        self.store
            .receive_staged(&stage.id, &reference, &bytes)
            .unwrap();
        let wrappers = self
            .store
            .retain_object_set(
                &stage.id,
                &format!(
                    "partial-checkpoint:producer:{}",
                    value.payload.publication_id
                ),
                &[
                    reference.clone(),
                    value.payload.q,
                    value.payload.source,
                    value.payload.runnable,
                ],
            )
            .unwrap();
        PartialCheckpointDataReceipt {
            format_version: 1,
            publication_id: value.payload.publication_id,
            publication: reference,
            wrappers,
        }
    }

    fn release_producers(&self) {
        for receipt in [
            &self.q.capture.retention.local,
            &self.q.evidence_retention,
            &self.receipt.wrappers,
        ] {
            self.store.release_object_set(receipt).unwrap();
        }
    }
}

fn export_fixture(f: &Fixture, directory: std::ffi::OsString) {
    use std::os::unix::fs::PermissionsExt;
    let data = partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).unwrap();
    let directory = std::path::PathBuf::from(directory);
    fs::create_dir_all(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let objects=data.objects().iter().map(|r|serde_json::json!({"reference":r,"hex":hex::encode(f.store.read_object(r).unwrap())})).collect::<Vec<_>>();
    let base_objects = f
        .base_objects
        .iter()
        .map(|(r, b)| serde_json::json!({"reference":r,"hex":hex::encode(b)}))
        .collect::<Vec<_>>();
    let payload = &data.publication().payload;
    let manifest = serde_json::json!({"format_version":1,"scope":"synthetic audit DATA, no physical/current authority",
            "receipt_json":serde_json::to_string(&f.receipt).unwrap(),"publication_id":f.receipt.publication_id,
            "publication":f.receipt.publication,"template_json":String::from_utf8(canonical_json(&f.template).unwrap()).unwrap(),
            "q_json":String::from_utf8(canonical_json(&f.q).unwrap()).unwrap(),
            "envelope_json":String::from_utf8(canonical_json(data.publication()).unwrap()).unwrap(),
            "plan_json":String::from_utf8(f.store.read_object(&payload.plan).unwrap()).unwrap(),
            "freeze_json":String::from_utf8(f.store.read_object(&payload.freeze_intent).unwrap()).unwrap(),
            "drain_json":String::from_utf8(f.store.read_object(&payload.drain_record).unwrap()).unwrap(),
            "source_json":String::from_utf8(f.store.read_object(&payload.source).unwrap()).unwrap(),
            "runnable_json":String::from_utf8(f.store.read_object(&payload.runnable).unwrap()).unwrap(),
            "base_objects":base_objects,"objects":objects});
    let temporary = directory.join(format!(".fixture-{}", Uuid::new_v4()));
    fs::write(&temporary, canonical_json(&manifest).unwrap()).unwrap();
    fs::rename(temporary, directory.join("fixture.json")).unwrap();
}

#[test]
fn canonical_data_fixture_preserves_u64_and_declared_edges() {
    let f = Fixture::new();
    let data = partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).unwrap();
    assert_eq!(data.q().capture.begin.generation, ABOVE_JS + 4);
    assert!(data.q().capture.retention.local.generation >= ABOVE_JS);
    assert!(f.receipt.wrappers.generation > ABOVE_JS);
    assert_eq!(data.publication().references.len(), 6);
    assert_eq!(
        data.publication()
            .payload
            .template
            .base_runnable
            .knowledge_cursor,
        ABOVE_JS + 6
    );
    let current = f.prepared.reconcile(&f.store).unwrap().unwrap();
    assert_eq!(
        PartialCheckpointDataReceipt::from_json(&current.transfer_receipt_json().unwrap()).unwrap(),
        f.receipt
    );
    assert!(f.store.retention().unwrap().pins.is_empty());
    assert!(f.store.read_object(&f.inherited).is_err());
    assert!(f.store.retained_closure(&f.receipt.publication).is_err());
    assert_eq!(
        f.q.capture.consistency,
        ObservationConsistency::LiveBestEffort
    );
    if let Some(directory) = std::env::var_os("RUDDER_PARTIAL_CHECKPOINT_FIXTURE_DIR") {
        export_fixture(&f, directory);
    }
}

#[test]
fn paged_directory_and_inherited_file_boundary_need_no_descriptor_payload_or_seed_hydration() {
    let f = Fixture::with_paged(true);
    let descriptor = f.inherited_descriptor.as_ref().unwrap();
    let data = partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).unwrap();
    let plan = rudder_engine::delta::read_partial_tree(&f.store, &f.q.capture.retention).unwrap();
    assert!(plan
        .local_objects
        .iter()
        .any(|r| r.kind == ObjectKind::DirectoryShard));
    assert!(plan
        .remote_boundaries
        .iter()
        .any(|p| p.reference == *descriptor && p.edges == [1, 0, 0]));
    let root = rudder_engine::directory::decode_directory_root(
        &f.store.read_object(&plan.directory).unwrap(),
    )
    .unwrap();
    let shard = rudder_engine::directory::decode_shard(
        &f.store.read_object(&root.directory_shard).unwrap(),
    )
    .unwrap();
    let rudder_engine::directory::ShardNode::Leaf { entries } = shard.node else {
        panic!("tiny fixture leaf")
    };
    assert!(entries
        .iter()
        .any(|entry| entry.name().unwrap() == b"\x80-old"));
    let mut session = TransferSession::open(&f.paths).unwrap();
    let inventory = session.pin_partial_checkpoint(&f.receipt).unwrap();
    assert_eq!(inventory.objects, data.objects());
    for reference in [
        descriptor,
        &f.inherited,
        &f.template.selection.accepted_seed,
    ] {
        assert!(!inventory.objects.contains(reference));
        assert!(f.store.read_object(reference).is_err());
        assert!(session.read(reference).is_err());
    }
    if let Some(directory) = std::env::var_os("RUDDER_PARTIAL_CHECKPOINT_PAGED_FIXTURE_DIR") {
        export_fixture(&f, directory);
    }
    f.release_producers();
    assert!(!f.store.gc(Duration::ZERO).unwrap().blocked_by_staging);
    for reference in &inventory.objects {
        session.read(reference).unwrap();
    }
    session.finish().unwrap();
}

#[test]
fn transfer_clone_owns_exact_union_after_producers_retire_and_gc() {
    let f = Fixture::new();
    let data = partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).unwrap();
    let mut session = TransferSession::open(&f.paths).unwrap();
    assert!(session.read(&f.receipt.publication).is_err());
    let inventory = session.pin_partial_checkpoint(&f.receipt).unwrap();
    assert!(inventory
        .owned_pin
        .starts_with("transfer:partial-checkpoint:rpc:"));
    assert_eq!(inventory.objects, data.objects());
    assert_eq!(inventory.publication_id, f.receipt.publication_id);
    assert_eq!(inventory.publication, f.receipt.publication);
    assert_eq!(inventory.base, f.template.selection.root);
    let value = serde_json::to_value(&inventory).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 12);
    assert!(value.get("receipt").is_none());
    assert!(value.get("generation").is_none());
    let clone = f
        .store
        .object_set_receipt(&inventory.owned_pin)
        .unwrap()
        .unwrap();
    assert!(clone.generation > f.receipt.wrappers.generation);
    assert_ne!(clone.staging_id, f.receipt.wrappers.staging_id);
    assert_eq!(f.store.object_set_refs(&clone).unwrap(), inventory.objects);
    assert!(session.unpin_source(&f.receipt.wrappers.pin_id).is_err());
    assert!(session.inventory(&f.receipt.publication).is_err());
    assert!(session.pin_source(&inventory.runnable).is_err());
    f.release_producers();
    let gc = f.store.gc(Duration::ZERO).unwrap();
    assert!(!gc.blocked_by_staging);
    for object in &inventory.objects {
        assert_eq!(
            session.read(object).unwrap(),
            f.store.read_object(object).unwrap()
        );
    }
    assert!(session.read(&f.inherited).is_err());
    assert!(session.read(&f.template.selection.accepted_seed).is_err());
    assert!(f.store.read_object(&f.inherited).is_err());
    assert!(f.store.retention().unwrap().pins.is_empty());
    session.unpin_source(&inventory.owned_pin).unwrap();
    assert!(session.read(&inventory.publication).is_err());
    session.finish().unwrap();
    f.store.gc(Duration::ZERO).unwrap();
    for object in &inventory.objects {
        assert!(f.store.read_object(object).is_err());
    }
}

#[test]
fn session_cleanup_releases_its_clone_and_preserves_all_producer_receipts() {
    let f = Fixture::new();
    let pin = {
        let mut session = TransferSession::open(&f.paths).unwrap();
        let inventory = session.pin_partial_checkpoint(&f.receipt).unwrap();
        let pin = inventory.owned_pin;
        session.finish().unwrap();
        session.finish().unwrap();
        pin
    };
    assert!(f.store.object_set_receipt(&pin).unwrap().is_none());
    assert!(partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).is_ok());
    assert!(f.store.retention().unwrap().staging.is_empty());
}

#[test]
fn failed_clone_exposes_no_reads_and_does_not_retire_producers_or_leak_staging() {
    let f = Fixture::new();
    let mut session = TransferSession::open(&f.paths).unwrap();
    for group in 0..3 {
        let mut receipt = f.receipt.clone();
        if group == 0 {
            receipt.wrappers.generation += 1;
        } else {
            let mut q = f.q.clone();
            if group == 1 {
                q.capture.retention.local.generation += 1;
            } else {
                q.evidence_retention.generation += 1;
            }
            assert!(f.prepare(&q).is_err());
            // The transfer entry point also checks each embedded original ACK.
            let data =
                partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).unwrap();
            let mut publication = data.publication().clone();
            let stage = f.store.begin_staging(Duration::from_secs(60)).unwrap();
            let (reference, bytes) = metadata(ObjectKind::Blob, &q);
            f.store
                .receive_staged(&stage.id, &reference, &bytes)
                .unwrap();
            f.store
                .retain_object_set(
                    &stage.id,
                    &format!("fixture:invalid-q:{group}"),
                    &[reference.clone()],
                )
                .unwrap();
            publication.payload.q = reference;
            receipt = f.with_publication(publication);
        }
        assert!(
            session.pin_partial_checkpoint(&receipt).is_err(),
            "group {group}"
        );
        assert!(session.read(&receipt.publication).is_err());
        assert!(f.store.retention().unwrap().staging.is_empty());
    }
    assert!(partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).is_ok());
    session.finish().unwrap();
}

#[test]
fn stale_transfer_and_producer_receipts_cannot_release_or_adopt_replacements() {
    let f = Fixture::new();
    let mut session = TransferSession::open(&f.paths).unwrap();
    let inventory = session.pin_partial_checkpoint(&f.receipt).unwrap();
    let first = f
        .store
        .object_set_receipt(&inventory.owned_pin)
        .unwrap()
        .unwrap();
    f.store.release_object_set(&first).unwrap();
    let stage = f.store.begin_staging(Duration::from_secs(60)).unwrap();
    let replacement = f
        .store
        .retain_object_set(&stage.id, &inventory.owned_pin, &inventory.objects)
        .unwrap();
    assert!(session.unpin_source(&inventory.owned_pin).is_err());
    assert!(session.finish().is_err());
    assert_eq!(
        f.store.object_set_refs(&replacement).unwrap(),
        inventory.objects
    );
    f.store.release_object_set(&replacement).unwrap();
    session.unpin_source(&inventory.owned_pin).unwrap();
    session.finish().unwrap();

    let refs = f.store.object_set_refs(&f.receipt.wrappers).unwrap();
    let intent = f.prepared.intent().unwrap();
    f.store.release_object_set(&f.receipt.wrappers).unwrap();
    let stage = f.store.begin_staging(Duration::from_secs(60)).unwrap();
    let replacement = f
        .store
        .retain_object_set(&stage.id, &f.receipt.wrappers.pin_id, &refs)
        .unwrap();
    assert!(f.prepared.reconcile(&f.store).is_err());
    assert!(partial_checkpoint::reconcile_partial_checkpoint_data(
        &f.store,
        &intent,
        &f.q,
        &f.template
    )
    .is_err());
    assert!(f.prepared.retain(&f.store).is_err());
    assert!(f.store.release_object_set(&f.receipt.wrappers).is_err());
    assert_eq!(f.store.object_set_refs(&replacement).unwrap(), refs);
}

#[test]
fn original_staging_identity_reconciles_lost_wrapper_ack_without_rewriting_bytes() {
    let f = Fixture::new();
    let mut state = f.store.retention().unwrap();
    // Restore the conservative real crash phase: metadata committed but the
    // original wrapper staging removal was not durably acknowledged.
    let stage = f.store.begin_staging(Duration::from_secs(60)).unwrap();
    state.staging.insert(
        f.receipt.wrappers.staging_id.clone(),
        rudder_engine::store::StagingLease {
            id: f.receipt.wrappers.staging_id.clone(),
            expires_at_ms: stage.expires_at_ms,
        },
    );
    state.generation += 1;
    let checksum = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&state).unwrap()).digest;
    fs::write(
        f.store.root().join("retention.json"),
        canonical_json(&serde_json::json!({"state":state,"checksum":checksum})).unwrap(),
    )
    .unwrap();
    assert!(f.store.gc(Duration::ZERO).unwrap().blocked_by_staging);
    let current = f.prepared.reconcile(&f.store).unwrap().unwrap();
    assert_eq!(
        PartialCheckpointDataReceipt::from_json(&current.transfer_receipt_json().unwrap()).unwrap(),
        f.receipt
    );
    assert!(f.prepared.retain(&f.store).is_err());
    // Reconciliation never discards conservative staging as a side effect.
    assert!(f
        .store
        .retention()
        .unwrap()
        .staging
        .contains_key(&f.receipt.wrappers.staging_id));
    f.store
        .abandon_staging(&f.receipt.wrappers.staging_id)
        .unwrap();
    f.store.abandon_staging(&stage.id).unwrap();
}

#[test]
fn lost_ack_recovery_survives_prepared_drop_store_reopen_and_expired_original_stage() {
    let f = Fixture::new();
    let intent = f.prepared.intent().unwrap();
    let bytes = canonical_json(&intent).unwrap();
    let intent = PartialCheckpointDataIntent::from_bytes(&bytes).unwrap();
    assert!(PartialCheckpointDataIntent::from_bytes(
        format!(" {}", String::from_utf8(bytes.clone()).unwrap()).as_bytes()
    )
    .is_err());
    let duplicate = String::from_utf8(bytes).unwrap().replacen(
        "\"format_version\":1",
        "\"format_version\":1,\"format_version\":1",
        1,
    );
    assert!(PartialCheckpointDataIntent::from_bytes(duplicate.as_bytes()).is_err());
    drop(f.prepared);
    // The original ACK is durable but not held by a live Prepared object.
    let mut state = f.store.retention().unwrap();
    state.staging.insert(
        intent.staging_id.clone(),
        rudder_engine::store::StagingLease {
            id: intent.staging_id.clone(),
            expires_at_ms: 1,
        },
    );
    state.generation += 1;
    let checksum = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&state).unwrap()).digest;
    fs::write(
        f.store.root().join("retention.json"),
        canonical_json(&serde_json::json!({"state":state,"checksum":checksum})).unwrap(),
    )
    .unwrap();
    let reopened = LocalStore::open(f.store.root()).unwrap();
    let current = partial_checkpoint::reconcile_partial_checkpoint_data(
        &reopened,
        &intent,
        &f.q,
        &f.template,
    )
    .unwrap()
    .unwrap();
    assert_eq!(current.publication_id(), f.receipt.publication_id);
    assert_eq!(current.publication(), &f.receipt.publication);
    let observed =
        PartialCheckpointDataReceipt::from_json(&current.transfer_receipt_json().unwrap()).unwrap();
    assert_eq!(observed, f.receipt);
    assert_eq!(
        reopened.retention().unwrap().staging[&intent.staging_id].expires_at_ms,
        1
    );
    let mut session = TransferSession::open(&f.paths).unwrap();
    let inventory = session.pin_partial_checkpoint(&observed).unwrap();
    assert_ne!(
        reopened
            .object_set_receipt(&inventory.owned_pin)
            .unwrap()
            .unwrap()
            .staging_id,
        intent.staging_id
    );
    session.finish().unwrap();
}

#[test]
fn recovery_without_ack_does_not_write_wrappers_or_consume_original_stage() {
    let f = Fixture::new();
    f.store.release_object_set(&f.receipt.wrappers).unwrap();
    let stage = f.store.begin_staging(Duration::from_secs(60)).unwrap();
    let prepared =
        partial_checkpoint::prepare_partial_checkpoint_data(&f.store, &stage.id, &f.q, &f.template)
            .unwrap();
    let intent = prepared.intent().unwrap();
    drop(prepared);
    let before = f.store.retention().unwrap();
    assert!(partial_checkpoint::reconcile_partial_checkpoint_data(
        &f.store,
        &intent,
        &f.q,
        &f.template
    )
    .unwrap()
    .is_none());
    assert_eq!(f.store.retention().unwrap().staging, before.staging);
    assert!(f
        .store
        .object_set_receipt(&f.receipt.wrappers.pin_id)
        .unwrap()
        .is_none());
    let mut mismatched = intent.clone();
    mismatched.publication.digest = "c".repeat(64);
    assert!(partial_checkpoint::reconcile_partial_checkpoint_data(
        &f.store,
        &mismatched,
        &f.q,
        &f.template
    )
    .is_err());
    assert_eq!(f.store.retention().unwrap().staging, before.staging);
    f.store.abandon_staging(&stage.id).unwrap();
}

#[test]
fn unknown_same_stage_aba_is_current_data_only_and_known_old_receipt_stays_stale() {
    let f = Fixture::new();
    let intent = f.prepared.intent().unwrap();
    let refs = f.store.object_set_refs(&f.receipt.wrappers).unwrap();
    f.store.release_object_set(&f.receipt.wrappers).unwrap();
    // Explicitly reproduce the conservative crash-stage ambiguity. A pre-ACK
    // intent cannot identify a first generation once someone releases/reuses
    // the same operation UUID; recovery must name only CURRENT same-op DATA.
    let expiry = f.store.begin_staging(Duration::from_secs(60)).unwrap();
    let mut state = f.store.retention().unwrap();
    state.staging.insert(
        intent.staging_id.clone(),
        rudder_engine::store::StagingLease {
            id: intent.staging_id.clone(),
            expires_at_ms: expiry.expires_at_ms,
        },
    );
    state.generation += 1;
    let checksum = ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&state).unwrap()).digest;
    fs::write(
        f.store.root().join("retention.json"),
        canonical_json(&serde_json::json!({"state":state,"checksum":checksum})).unwrap(),
    )
    .unwrap();
    let replacement = f
        .store
        .retain_object_set(&intent.staging_id, &f.receipt.wrappers.pin_id, &refs)
        .unwrap();
    assert!(replacement.generation > f.receipt.wrappers.generation);
    assert!(partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).is_err());
    let current =
        partial_checkpoint::reconcile_partial_checkpoint_data(&f.store, &intent, &f.q, &f.template)
            .unwrap()
            .unwrap();
    let observed =
        PartialCheckpointDataReceipt::from_json(&current.transfer_receipt_json().unwrap()).unwrap();
    assert_eq!(observed.wrappers, replacement);
    assert!(f.store.release_object_set(&f.receipt.wrappers).is_err());
    assert_eq!(f.store.object_set_refs(&replacement).unwrap(), refs);
    f.store.abandon_staging(&expiry.id).unwrap();
}

#[test]
fn exact_session_pin_admission_is_bounded_and_recovers_after_release() {
    let f = Fixture::new();
    let mut session = TransferSession::open(&f.paths).unwrap();
    let mut pins = Vec::new();
    for _ in 0..32 {
        pins.push(
            session
                .pin_partial_checkpoint(&f.receipt)
                .unwrap()
                .owned_pin,
        );
    }
    assert!(session.pin_partial_checkpoint(&f.receipt).is_err());
    assert!(f.store.retention().unwrap().staging.is_empty());
    session.unpin_source(&pins.remove(0)).unwrap();
    assert!(session.pin_partial_checkpoint(&f.receipt).is_ok());
    session.finish().unwrap();
    assert!(partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).is_ok());
}

#[test]
fn resealed_freeze_and_drain_field_mismatches_are_rejected_independently() {
    let f = Fixture::new();
    let freeze: serde_json::Value =
        serde_json::from_slice(&f.store.read_object(&f.q.freeze_intent).unwrap()).unwrap();
    let drain: serde_json::Value =
        serde_json::from_slice(&f.store.read_object(&f.q.drain_record).unwrap()).unwrap();
    for (pointer, replacement) in [
        ("/format_version", serde_json::json!(2)),
        (
            "/execution_id",
            serde_json::json!("99999999-9999-9999-9999-999999999999"),
        ),
        (
            "/operation_id",
            serde_json::json!("99999999-9999-9999-9999-999999999999"),
        ),
        (
            "/view_id",
            serde_json::json!("99999999-9999-9999-9999-999999999999"),
        ),
        ("/owner/epoch", serde_json::json!(ABOVE_JS + 1)),
        (
            "/execution_request_digest",
            serde_json::json!("c".repeat(64)),
        ),
        ("/launch_request_digest", serde_json::json!("c".repeat(64))),
        ("/stop/run_epoch", serde_json::json!(ABOVE_JS + 3)),
        ("/partial_binding_digest", serde_json::json!("c".repeat(64))),
        ("/endpoint_instance", serde_json::json!("invalid")),
        ("/network/format_version", serde_json::json!(2)),
        ("/network/profile_digest", serde_json::json!("invalid")),
        ("/network/network_id", serde_json::json!("invalid")),
        ("/network/readiness_nonce", serde_json::json!("invalid")),
        (
            "/network/mode",
            serde_json::json!("qualification_https_connect_v1"),
        ),
        ("/kernel_drained", serde_json::json!(false)),
    ] {
        let mut changed = drain.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        let q = f.with_evidence(&freeze, &changed);
        q.validate_data().unwrap();
        assert!(f.prepare(&q).is_err(), "drain {pointer}");
    }
    for (pointer, replacement) in [
        ("/format_version", serde_json::json!(2)),
        ("/launch_digest", serde_json::json!("c".repeat(64))),
        ("/request/format_version", serde_json::json!(2)),
        ("/request/intent_id", serde_json::json!(f.q.stop.request_id)),
        (
            "/request/capture/nonce",
            serde_json::json!("99999999-9999-9999-9999-999999999999"),
        ),
        (
            "/request/capture/expected_begin/generation",
            serde_json::json!(ABOVE_JS + 5),
        ),
    ] {
        let mut changed = freeze.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        let q = f.with_evidence(&changed, &drain);
        q.validate_data().unwrap();
        assert!(f.prepare(&q).is_err(), "freeze {pointer}");
    }
}

#[test]
fn resealed_plan_provenance_is_bounded_disjoint_and_sorted() {
    let f = Fixture::new();
    let plan = rudder_engine::delta::read_partial_tree(&f.store, &f.q.capture.retention).unwrap();
    let mut cases = Vec::new();
    let mut changed = plan.clone();
    changed.local_objects.reverse();
    cases.push(changed);
    let mut changed = plan.clone();
    changed.local_objects.push(changed.local_objects[0].clone());
    cases.push(changed);
    let mut changed = plan.clone();
    changed.remote_boundaries.reverse();
    cases.push(changed);
    let mut changed = plan.clone();
    changed
        .remote_boundaries
        .push(changed.remote_boundaries[0].clone());
    cases.push(changed);
    let mut changed = plan.clone();
    changed.remote_boundaries[0].edges = vec![0; 1025];
    cases.push(changed);
    let mut changed = plan.clone();
    changed.remote_boundaries.push(RemoteObjectPath {
        reference: changed.local_objects[0].clone(),
        edges: vec![0],
    });
    changed
        .remote_boundaries
        .sort_by(|a, b| a.reference.cmp(&b.reference));
    cases.push(changed);
    let mut changed = plan.clone();
    changed.local_objects.push(f.receipt.publication.clone());
    changed.local_objects.sort();
    cases.push(changed);
    let mut changed = plan.clone();
    changed.directory = ObjectRef::from_bytes(ObjectKind::Directory, b"no provenance");
    cases.push(changed);
    let mut changed = plan.clone();
    changed
        .remote_boundaries
        .retain(|p| p.reference != plan.hardlinks);
    cases.push(changed);
    for (index, plan) in cases.iter().enumerate() {
        let q = f.with_plan(plan);
        q.validate_data().unwrap();
        assert!(f.prepare(&q).is_err(), "plan case {index}");
    }
}

#[test]
fn codec_rejects_purpose_reference_duplicates_unknowns_and_integer_overflow() {
    let f = Fixture::new();
    let data = partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).unwrap();
    let publication = data.publication();
    let canonical = String::from_utf8(canonical_json(publication).unwrap()).unwrap();
    let duplicate = canonical.replacen(
        "\"format_version\":1",
        "\"format_version\":1,\"format_version\":1",
        1,
    );
    assert!(PartialCheckpointPublicationV1::from_bytes(duplicate.as_bytes()).is_err());
    let unknown = canonical.replacen("{", "{\"unknown\":0,", 1);
    assert!(PartialCheckpointPublicationV1::from_bytes(unknown.as_bytes()).is_err());
    assert!(
        PartialCheckpointPublicationV1::from_bytes(format!(" {canonical}").as_bytes()).is_err()
    );
    let opaque = serde_json::to_string(&f.receipt).unwrap();
    assert_eq!(
        PartialCheckpointDataReceipt::from_json(&opaque).unwrap(),
        f.receipt
    );
    let overflow = opaque.replace(
        &f.receipt.wrappers.generation.to_string(),
        "18446744073709551616",
    );
    assert!(PartialCheckpointDataReceipt::from_json(&overflow).is_err());
    let outside_ledger = opaque.replace(
        &f.receipt.wrappers.generation.to_string(),
        "18446744073709551615",
    );
    assert!(PartialCheckpointDataReceipt::from_json(&outside_ledger).is_err());
    for kind in ["partial-tree-v1", "checkpoint", "source-jj-projection-v1"] {
        let mut invalid = publication.clone();
        invalid.payload.kind = kind.into();
        invalid.payload.publication_id = invalid.payload.expected_publication_id().unwrap();
        assert!(invalid.validate_data().is_err());
    }
    let mut invalid = publication.clone();
    invalid.payload.q.size_bytes = partial_checkpoint::MAX_Q_BYTES as u64 + 1;
    invalid.payload.publication_id = invalid.payload.expected_publication_id().unwrap();
    invalid.references = invalid.payload.references().unwrap();
    assert!(invalid.validate_data().is_err());
    let mut invalid = publication.clone();
    invalid.references.reverse();
    assert!(invalid.validate_data().is_err());
    let mut invalid = publication.clone();
    invalid.references.push(invalid.references[0].clone());
    assert!(invalid.validate_data().is_err());
}

#[test]
fn exact_template_scope_base_and_source_only_profile_are_required() {
    let f = Fixture::new();
    for change in 0..7 {
        let mut template = f.template.clone();
        match change {
            0 => template.selection.root.scope.tenant_id = "different-tenant".into(),
            1 => template.selection.root.root_id = "different-base-id".into(),
            2 => template.selection.attempt.run_epoch += 1,
            3 => template.selection.projection_policy = f.q.freeze_intent.clone(),
            4 => template.selection.accepted_source = f.receipt.publication.clone(),
            5 => template.selection.observation.profile = f.q.freeze_intent.clone(),
            _ => template.selection.observation.owner.epoch += 1,
        }
        let stage = f.store.begin_staging(Duration::from_secs(60)).unwrap();
        assert!(
            partial_checkpoint::prepare_partial_checkpoint_data(
                &f.store, &stage.id, &f.q, &template
            )
            .is_err(),
            "binding case {change}"
        );
        f.store.abandon_staging(&stage.id).unwrap();
    }
    let rebind = |template: &mut PartialCheckpointTemplateV1| {
        template.base_runnable.profile = metadata(ObjectKind::Blob, &template.profile).0;
        template.selection.observation.profile = template.base_runnable.profile.clone();
        template.base_runnable.source = metadata(ObjectKind::SourceRoot, &template.base_source).0;
        template.selection.accepted_source = template.base_runnable.source.clone();
        template.selection.root.root =
            metadata(ObjectKind::RunnableRoot, &template.base_runnable).0;
    };
    for change in 0..4 {
        let mut template = f.template.clone();
        match change {
            0 => template.profile.prepared = vec![vec![b"dependencies".to_vec()]],
            1 => {
                template.base_runnable.environment = Some(ObjectRef::from_bytes(
                    ObjectKind::EnvironmentRoot,
                    b"environment",
                ))
            }
            2 => {
                template.base_source.directory =
                    ObjectRef::from_bytes(ObjectKind::Directory, b"different directory")
            }
            _ => {
                template.base_runnable.hardlinks =
                    ObjectRef::from_bytes(ObjectKind::Blob, b"nonempty topology")
            }
        }
        rebind(&mut template);
        assert!(template.validate_data().is_err(), "template case {change}");
    }
}

#[test]
fn oversized_plan_is_rejected_from_its_ref_without_an_unbounded_read() {
    let f = Fixture::new();
    let mut plan =
        rudder_engine::delta::read_partial_tree(&f.store, &f.q.capture.retention).unwrap();
    plan.capture_id = "x".repeat(4 * 1024 * 1024);
    let q = f.with_plan(&plan);
    assert!(q.capture.retention.plan.size_bytes > 4 * 1024 * 1024);
    let error = f.prepare(&q).unwrap_err().to_string();
    assert!(error.contains("invalid partial-tree receipt"), "{error}");
}

#[test]
fn resealed_envelope_cannot_substitute_unbound_q_or_rewrite_source_policy() {
    let f = Fixture::new();
    let data = partial_checkpoint::read_partial_checkpoint_data(&f.store, &f.receipt).unwrap();
    let mut publication = data.publication().clone();
    let mut unbound_q = f.q.clone();
    unbound_q.execution_request_digest = "c".repeat(64);
    unbound_q.validate_data().unwrap();
    let (reference, bytes) = metadata(ObjectKind::Blob, &unbound_q);
    let stage = f.store.begin_staging(Duration::from_secs(60)).unwrap();
    f.store
        .receive_staged(&stage.id, &reference, &bytes)
        .unwrap();
    f.store
        .retain_object_set(&stage.id, "fixture:unbound-q", &[reference.clone()])
        .unwrap();
    publication.payload.q = reference;
    let changed = f.with_publication(publication);
    assert!(partial_checkpoint::read_partial_checkpoint_data(&f.store, &changed).is_err());

    let mut source = f.template.base_source.clone();
    let plan = rudder_engine::delta::read_partial_tree(&f.store, &f.q.capture.retention).unwrap();
    source.directory = plan.directory.clone();
    source.projection_policy = f.q.freeze_intent.clone();
    let source_object = metadata(ObjectKind::SourceRoot, &source);
    let mut runnable = f.template.base_runnable.clone();
    runnable.source = source_object.0.clone();
    runnable.view_root = plan.directory;
    let runnable_object = metadata(ObjectKind::RunnableRoot, &runnable);
    let stage = f.store.begin_staging(Duration::from_secs(60)).unwrap();
    for (reference, bytes) in [&source_object, &runnable_object] {
        f.store.receive_staged(&stage.id, reference, bytes).unwrap();
    }
    f.store
        .retain_object_set(
            &stage.id,
            "fixture:wrong-policy",
            &[source_object.0.clone(), runnable_object.0.clone()],
        )
        .unwrap();
    let mut publication = data.publication().clone();
    publication.payload.source = source_object.0;
    publication.payload.runnable = runnable_object.0;
    let changed = f.with_publication(publication);
    let error = partial_checkpoint::read_partial_checkpoint_data(&f.store, &changed)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("exact source-complement DATA"), "{error}");
}

#[test]
#[ignore = "requires explicit freshly built RUDDER_PARTIAL_CHECKPOINT_NATIVE_BIN"]
fn actual_native_rpc_clones_opaque_checkpoint_receipt_and_settles_eof() {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::Instant,
    };
    let native = std::env::var_os("RUDDER_PARTIAL_CHECKPOINT_NATIVE_BIN")
        .expect("explicit fresh Native artifact path is required");
    let f = Fixture::new();
    struct Process {
        child: Child,
        reader: Option<thread::JoinHandle<()>>,
    }
    impl Drop for Process {
        fn drop(&mut self) {
            self.child.stdin.take();
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(reader) = self.reader.take() {
                reader.join().unwrap();
            }
        }
    }
    let mut child = Command::new(native)
        .args(["fabric", "rpc"])
        .env_clear()
        .env("RUDDER_HOME", f._root.path())
        .current_dir(f._root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let output = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut output = BufReader::new(output);
        loop {
            let mut line = String::new();
            if !output.read_line(&mut line).is_ok_and(|bytes| bytes > 0) {
                break;
            }
            let Ok(header) = serde_json::from_str::<serde_json::Value>(&line) else {
                break;
            };
            let Some(count) = header["binary_bytes"]
                .as_u64()
                .filter(|n| *n <= 256 * 1024 * 1024)
            else {
                break;
            };
            let mut bytes = vec![0; count as usize];
            if output.read_exact(&mut bytes).is_err() || sender.try_send((header, bytes)).is_err() {
                break;
            }
        }
    });
    let mut process = Process {
        child,
        reader: Some(reader),
    };
    let call = |child: &mut Child, operation: serde_json::Value| {
        let id = Uuid::new_v4().to_string();
        let frame =
            serde_json::json!({"format_version":1,"id":id,"request":operation,"binary_bytes":0});
        let input = child.stdin.as_mut().unwrap();
        writeln!(input, "{frame}").unwrap();
        input.flush().unwrap();
        let (header, bytes) = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("bounded Native frame response");
        assert_eq!(header["id"], id);
        (header, bytes)
    };
    let (header, bytes) = call(
        &mut process.child,
        serde_json::json!({
            "operation":"pin_partial_checkpoint","receipt_json":serde_json::to_string(&f.receipt).unwrap()
        }),
    );
    assert!(bytes.is_empty());
    assert_eq!(header["ok"], true, "{header}");
    assert_eq!(header["result"].as_object().unwrap().len(), 12);
    let pin = header["result"]["owned_pin"].as_str().unwrap().to_owned();
    assert!(pin.starts_with("transfer:partial-checkpoint:rpc:"));
    let objects: Vec<ObjectRef> =
        serde_json::from_value(header["result"]["objects"].clone()).unwrap();
    let clone = f.store.object_set_receipt(&pin).unwrap().unwrap();
    assert!(clone.generation > ABOVE_JS);
    assert_ne!(clone.staging_id, f.receipt.wrappers.staging_id);
    f.release_producers();
    assert!(!f.store.gc(Duration::ZERO).unwrap().blocked_by_staging);
    for object in &objects {
        let (header, bytes) = call(
            &mut process.child,
            serde_json::json!({"operation":"read_binary","object":object}),
        );
        assert_eq!(header["ok"], true, "{header}");
        assert_eq!(bytes, f.store.read_object(object).unwrap());
    }
    let (header, _) = call(
        &mut process.child,
        serde_json::json!({"operation":"inventory","root":f.receipt.publication}),
    );
    assert_eq!(header["ok"], false);
    process.child.stdin.take();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = process.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "Native EOF did not settle");
        thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success(), "{status}");
    assert!(f.store.object_set_receipt(&pin).unwrap().is_none());
    println!("actual Native checkpoint RPC: child exit={status}; EOF/wait settled; >JS53 receipt preserved");
}
