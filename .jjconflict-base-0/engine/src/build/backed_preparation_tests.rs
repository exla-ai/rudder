//! Real CAS/files/SQLite component fixtures, not Root/SDK/project grants.
//! All definitions are UNRUN by the author. No compiler/backend is launched.
use super::*;
use crate::{content::{ContentStore, FileNode, FILE_LAYOUT},
    directory::{DirectoryEntry, EntryNode, LocalDirectoryStore},
    store::{LocalImageImport, RetentionState,
        baked_image::{BakedImageCas, BakedImageInventoryData, BakedImageRoots, BAKED_IMAGE_LAYOUT}}};
use sha2::{Digest, Sha256};
use std::{os::{fd::OwnedFd, unix::fs::PermissionsExt}, panic::{catch_unwind, AssertUnwindSafe},
    sync::Weak, time::Instant};

struct Clock(FinalizationBudget);
impl Clock { fn new() -> Arc<Self> { Arc::new(Self(budget())) } }
impl ImageImportFence for Clock { fn remaining(&self) -> Result<Duration> { self.0.remaining() } }
fn budget() -> FinalizationBudget { FinalizationBudget::begin(Duration::from_secs(30)).unwrap() }
fn cancel() -> Arc<AtomicBool> { Arc::new(AtomicBool::new(false)) }

struct Fixture {
    temporary: tempfile::TempDir, baked_root: PathBuf, index: Arc<BuildIndex>,
    source: Arc<VerifiedBakedInputs>, spec: PreparationSpec, anchors: Vec<ObjectRef>,
    payload: ObjectRef, payload_bytes: Vec<u8>, manifest: ObjectRef,
}
impl Fixture {
    fn new() -> Self {
        let temporary = if Path::new("/workspace").is_dir() { tempfile::tempdir_in("/workspace").unwrap() }
            else { tempfile::tempdir().unwrap() };
        let baked_root = temporary.path().join("baked"); fs::create_dir(&baked_root).unwrap();
        let backing = LocalStore::open(baked_root.join("cas")).unwrap();
        let mut import = None; LocalImageImport::stage_retaining(&mut import, backing.clone(), Clock::new()).unwrap();
        let import = import.unwrap();
        let payload_bytes = vec![b'X'; 4 * 1024 * 1024];
        let payload = ContentStore::put_blob(&import, &payload_bytes).unwrap();
        let tail = ContentStore::put_blob(&import, b"real-tail").unwrap();
        let file = ContentStore::put_file_node(&import, &FileNode { format_version: 1,
            layout: FILE_LAYOUT.into(), level: 0, logical_size: payload.size_bytes + tail.size_bytes,
            children: vec![payload.clone(), tail.clone()] }).unwrap();
        let topology = ContentStore::put_blob(&import, b"{\"format_version\":1,\"groups\":[]}").unwrap();
        let installer = PagedDirectory::new(&import).build(0o555, vec![
            DirectoryEntry::new(b"hardlinks.json", EntryNode::File { object: topology.clone(),
                logical_size: topology.size_bytes, mode: 0o444 }).unwrap(),
            DirectoryEntry::new(b"tool", EntryNode::File { object: file.clone(),
                logical_size: payload.size_bytes + tail.size_bytes, mode: 0o555 }).unwrap(),
        ]).unwrap();
        let mut whole = Sha256::new(); whole.update(&payload_bytes); whole.update(b"real-tail");
        let manifest_bytes = canonical_json(&serde_json::json!({"format_version":1,
            "installer_input":installer,"files":[{"path":"/tool","sha256":hex::encode(whole.finalize()),
                "size_bytes":payload.size_bytes+tail.size_bytes,"content":file}]})).unwrap();
        let manifest = ObjectRef::from_bytes(ObjectKind::Blob, &manifest_bytes);
        import.receive_manifest(&manifest, &manifest_bytes).unwrap();
        let pins = import.commit_pins("fixture-installer", &installer, "fixture-manifest", &manifest).unwrap();
        let objects = import.objects_after_commit(&pins).unwrap(); let retention = backing.retention().unwrap();
        let roots = BakedImageRoots { format_version: 1, manifest: manifest.clone(), installer_input: installer.clone() };
        let raw_retention = fs::read(baked_root.join("cas/retention.json")).unwrap();
        let inventory = BakedImageInventoryData { format_version: 1, layout: BAKED_IMAGE_LAYOUT.into(),
            roots: roots.clone(), total_bytes: objects.iter().map(|r| r.size_bytes).sum(), objects,
            retention: ObjectRef::from_bytes(ObjectKind::Blob, &raw_retention),
            retention_generation: retention.generation, pins: retention.pins };
        let raw_inventory = canonical_json(&inventory).unwrap();
        let expected = ObjectRef::from_bytes(ObjectKind::Blob, &raw_inventory);
        fs::write(baked_root.join("inventory.json"), raw_inventory).unwrap();
        fs::write(baked_root.join("pin.json"), canonical_json(&roots).unwrap()).unwrap();
        fs::write(baked_root.join("manifest.json"), manifest_bytes).unwrap(); drop(import);
        for name in ["inventory.json", "pin.json", "manifest.json", "cas/lock", "cas/retention.json"] {
            fs::set_permissions(baked_root.join(name), fs::Permissions::from_mode(0o444)).unwrap();
        }
        for shard in fs::read_dir(baked_root.join("cas/objects")).unwrap() {
            fs::set_permissions(shard.unwrap().path(), fs::Permissions::from_mode(0o555)).unwrap();
        }
        for name in ["cas/objects", "cas/tmp", "cas", ""] {
            fs::set_permissions(baked_root.join(name), fs::Permissions::from_mode(0o555)).unwrap();
        }
        let root_fd: OwnedFd = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&baked_root).unwrap().into();
        let inventory_fd: OwnedFd = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(baked_root.join("inventory.json")).unwrap().into();
        let mut baked = None;
        BakedImageCas::capture_local_fixture_retaining(&mut baked, root_fd, inventory_fd, expected, Clock::new()).unwrap();
        let mut source = None;
        VerifiedBakedInputs::capture_retaining(&mut source, baked.unwrap(), Clock::new()).unwrap();
        let local = LocalStore::open(temporary.path().join("cas")).unwrap();
        let network = local.put_blob(b"actual per-VM network component DATA\n").unwrap();
        let model = local.put_directory(crate::store::DirectoryManifest {
            format_version: 1, mode: 0o555, entries: Vec::new() }).unwrap();
        local.pin("actual-network", &network).unwrap(); local.pin("actual-model", &model).unwrap();
        let spec = PreparationSpec { format_version: 1, base_runtime: installer.clone(),
            platform: PlatformIdentity { os: "linux".into(), architecture: "x86_64".into(), abi: "component-only".into(),
                properties: BTreeMap::from([("runtime_profile".into(), "backed-component-v1".into())]) },
            installer_inputs: BTreeMap::from([("image/full-tree".into(), installer.clone()),
                ("image/manifest".into(), manifest.clone()), ("vm/network".into(), network.clone()),
                ("model/closure".into(), model.clone())]),
            installer_commands: Vec::new(), installer_environment: BTreeMap::new(),
            toolchains: BTreeMap::from([("compiler".into(), ToolchainIdentity { version: "component-bytes-1".into(),
                closure: installer, executable: file, executable_path: "tool".into(), configuration: BTreeMap::new() })]),
            policy_version: "backed-component-v1".into(), effects: BTreeSet::new() };
        let index = Arc::new(BuildIndex::open(temporary.path().join("index"), local).unwrap());
        Self { temporary, baked_root, index, source: source.unwrap(), spec,
            anchors: vec![network, model], payload, payload_bytes, manifest }
    }
    fn run(&self, out: &mut Option<RetainedBackedPreparation>, prior: Option<&BackedPreparationAcknowledgment>) -> Result<()> {
        self.index.prepare_backed_retaining(out, self.spec.clone(), PreparedContentLayout::PagedChunkedV2,
            self.spec.base_runtime.clone(), self.anchors.clone(), vec![self.source.clone()], prior, budget(), cancel())
    }
    fn staged(&self, fault: api::TestPoint) -> RetainedBackedPreparation {
        let mut owner = RetainedBackedPreparation::stage_backed(self.index.clone(), self.spec.clone(),
            PreparedContentLayout::PagedChunkedV2, self.spec.base_runtime.clone(), self.anchors.clone(),
            vec![self.source.clone()], None, budget(), cancel()); owner.fault = Some(fault); owner
    }
    fn count(&self) -> i64 {
        let db = Connection::open(self.index.root.join("index.sqlite")).unwrap();
        db.query_row("SELECT count(*) FROM build_index WHERE category=?1", [BACKED_PREPARATION_CATEGORY], |r| r.get(0)).unwrap()
    }
    fn state(&self) -> RetentionState {
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(self.index.store.root().join("retention.json")).unwrap()).unwrap();
        serde_json::from_value(raw["state"].clone()).unwrap()
    }
    fn pin_name(&self, owner: &RetainedBackedPreparation) -> String { pin(BACKED_PREPARATION_CATEGORY, owner.key_data().unwrap()) }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fn writable(path: &Path) {
            let Ok(m) = fs::symlink_metadata(path) else { return; };
            if !m.is_dir() || m.file_type().is_symlink() { return; }
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
            if let Ok(entries) = fs::read_dir(path) { for entry in entries.flatten() { writable(&entry.path()); } }
        }
        writable(&self.baked_root);
    }
}

#[test]
fn backed_preparation_real_cas_sql_receipt_preserves_inputs_without_payload_copy() {
    let f = Fixture::new(); let original = budget(); let constraint = original.constraint(); let mut out = None;
    f.index.prepare_backed_retaining(&mut out, f.spec.clone(), PreparedContentLayout::PagedChunkedV2,
        f.spec.base_runtime.clone(), f.anchors.clone(), vec![f.source.clone()], None, original, cancel()).unwrap();
    let owner = out.as_ref().unwrap(); let ack = owner.acknowledgment().unwrap(); let prepared = ack.prepared();
    assert_eq!(owner.original_constraint_data(), constraint); assert!(!ack.reused()); assert_eq!(f.count(), 1);
    assert_eq!(owner.core.sql, SqlOutcome::Committed); assert!(!owner.core.key_locked);
    assert!(owner.core.store.as_ref().unwrap().gc.is_none()); assert!(owner.publication().unwrap().acknowledgment().is_ok());
    assert!(!f.index.store.object_path(&f.payload).unwrap().exists());
    let state = f.state(); assert!(!state.pins.values().any(|r| r == &prepared.receipt));
    assert_eq!(&state.backed_retention.unwrap().pins[&f.pin_name(owner)], ack.retention().record_data());
    let mut read = None; ack.retention().read_retaining(&mut read, prepared.root.clone(), Clock::new()).unwrap();
    let env = crate::snapshot::decode_environment(read.as_ref().unwrap().verified_bytes().unwrap()).unwrap();
    assert_eq!(env.directory, f.spec.base_runtime); assert_eq!(env.platform, f.spec.platform.properties());
    for root in f.spec.installer_inputs.values().chain(f.anchors.iter()) { assert!(env.installer_inputs.contains(root)); }
    assert!(env.installer_inputs.contains(&f.manifest));
    assert!(env.installer_inputs.iter().all(|r| matches!(r.kind, ObjectKind::Blob | ObjectKind::File | ObjectKind::Directory)));
    assert_eq!(env.provenance["preparation_category"], BACKED_PREPARATION_CATEGORY);
    // Manually copying DATA into the old type does not create an ordinary hit.
    let ordinary = PreparedEnvironment { key: prepared.key.clone(), root: prepared.root.clone(), receipt: prepared.receipt.clone() };
    assert!(f.index.validate_prepared(&ordinary).is_err());
}

#[test]
fn backed_preparation_warm_reuse_requires_same_actual_prior_ack_and_retains_it() {
    let f = Fixture::new(); let mut first = None; f.run(&mut first, None).unwrap();
    let ack = first.as_ref().unwrap().acknowledgment().unwrap(); let expected = ack.prepared().clone();
    let weak: Weak<crate::store::backed_retention::BackedPinOwner> = Arc::downgrade(ack.retention());
    let mut repeated = None; f.run(&mut repeated, Some(ack)).unwrap();
    assert!(repeated.as_ref().unwrap().acknowledgment().unwrap().reused());
    assert_eq!(repeated.as_ref().unwrap().acknowledgment().unwrap().prepared(), &expected);
    assert!(repeated.as_ref().unwrap().publication().is_none()); assert!(repeated.as_ref().unwrap().staging_id_data().is_none());
    drop(first); assert!(weak.upgrade().is_some()); assert_eq!(f.count(), 1);
    let mut read = None; repeated.as_ref().unwrap().acknowledgment().unwrap().retention()
        .read_retaining(&mut read, f.payload.clone(), Clock::new()).unwrap();
    assert_eq!(read.as_ref().unwrap().verified_bytes().unwrap(), &f.payload_bytes);
}

#[test]
fn backed_preparation_row_without_genuine_prior_owner_is_not_a_cache_ack() {
    let f = Fixture::new(); let mut first = None; f.run(&mut first, None).unwrap(); drop(first);
    let reopened = Arc::new(BuildIndex::open(f.index.root.clone(), f.index.store.clone()).unwrap()); let mut out = None;
    assert!(reopened.prepare_backed_retaining(&mut out, f.spec.clone(), PreparedContentLayout::PagedChunkedV2,
        f.spec.base_runtime.clone(), f.anchors.clone(), vec![f.source.clone()], None, budget(), cancel()).is_err());
    let owner = out.as_ref().unwrap(); assert!(owner.acknowledgment().is_err());
    assert!(owner.failure().unwrap().to_string().contains("row alone")); assert!(owner.core.store.is_none()); assert_eq!(f.count(), 1);
}

#[test]
fn backed_preparation_wrong_index_cannot_borrow_a_prior_ack_from_another_connection_owner() {
    let f = Fixture::new(); let mut first = None; f.run(&mut first, None).unwrap();
    let reopened = Arc::new(BuildIndex::open(f.index.root.clone(), f.index.store.clone()).unwrap()); let mut out = None;
    assert!(reopened.prepare_backed_retaining(&mut out, f.spec.clone(), PreparedContentLayout::PagedChunkedV2,
        f.spec.base_runtime.clone(), f.anchors.clone(), vec![f.source.clone()],
        Some(first.as_ref().unwrap().acknowledgment().unwrap()), budget(), cancel()).is_err());
    assert!(out.as_ref().unwrap().acknowledgment().is_err()); assert!(out.as_ref().unwrap().retention.is_none());
}

#[test]
fn backed_preparation_real_insert_refusal_keeps_genuine_retention_and_original_sql() {
    let f = Fixture::new(); let db = Connection::open(f.index.root.join("index.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER deny_backed BEFORE INSERT ON build_index BEGIN SELECT RAISE(ABORT,'actual-insert-refusal'); END;").unwrap();
    let mut out = None; assert!(f.run(&mut out, None).is_err()); let owner = out.as_ref().unwrap();
    assert_eq!(owner.core.sql, SqlOutcome::InsertAttempted); assert!(owner.core.key_locked);
    assert!(!owner.core.connection.as_ref().unwrap().is_autocommit());
    assert!(owner.publication().unwrap().acknowledgment().is_ok()); assert!(owner.retention.is_some());
    assert!(owner.acknowledgment().is_err()); assert_eq!(f.count(), 0);
    assert!(f.state().backed_retention.unwrap().pins.contains_key(&f.pin_name(owner)));
}

#[test]
fn backed_preparation_actual_commit_hook_refusal_keeps_pin_and_unknown_sql_carrier() {
    let f = Fixture::new(); let mut owner = f.staged(api::TestPoint::CommitHookRefusal);
    assert!(owner.run_backed().is_err()); assert_eq!(owner.core.sql, SqlOutcome::CommitAttempted);
    assert!(owner.core.connection.is_some()); assert!(owner.core.key_locked);
    assert!(owner.publication().unwrap().acknowledgment().is_ok()); assert!(owner.retention.is_some());
    assert!(owner.acknowledgment().is_err()); assert_eq!(f.count(), 0); assert!(owner.run_backed().is_err());
}

#[test]
fn backed_preparation_known_commit_lost_ack_cannot_resurrect_from_select() {
    let f = Fixture::new(); let mut owner = f.staged(api::TestPoint::AfterCommit);
    assert!(owner.run_backed().is_err()); assert_eq!(owner.core.sql, SqlOutcome::Committed); assert_eq!(f.count(), 1);
    assert!(owner.retention.is_some()); assert!(owner.acknowledgment().is_err()); assert!(owner.run_backed().is_err());
    // Simulated process loss, not a production cleanup/retry API. The durable
    // row+pin still cannot supply a new original acknowledgment after reopen.
    drop(owner); let mut next = None; assert!(f.run(&mut next, None).is_err());
    assert!(next.as_ref().unwrap().acknowledgment().is_err()); assert!(next.as_ref().unwrap().core.store.is_none());
}

#[test]
fn backed_preparation_retention_and_insert_outcomes_stay_owned_without_sql_ack() {
    for point in [api::TestPoint::AfterRetention, api::TestPoint::AfterInsert] {
        let f = Fixture::new(); let mut owner = f.staged(point); assert!(owner.run_backed().is_err());
        assert!(owner.publication().unwrap().acknowledgment().is_ok()); assert!(owner.retention.is_some());
        assert!(owner.core.key_locked); assert!(!owner.core.connection.as_ref().unwrap().is_autocommit());
        assert!(owner.acknowledgment().is_err()); assert_eq!(f.count(), 0); assert!(owner.run_backed().is_err());
    }
}

#[test]
fn backed_preparation_late_retention_or_commit_never_renews_original_budget() {
    for point in [api::TestPoint::CancelAfterRetention, api::TestPoint::CancelAfterCommit] {
        let f = Fixture::new(); let mut owner = f.staged(point); let constraint = owner.original_constraint_data();
        assert!(owner.run_backed().is_err()); assert_eq!(owner.original_constraint_data(), constraint);
        assert!(owner.core.original.remaining().is_err()); assert!(owner.retention.is_some());
        assert!(owner.publication().unwrap().acknowledgment().is_ok()); assert!(owner.acknowledgment().is_err());
        assert_eq!(f.count(), if point == api::TestPoint::CancelAfterCommit { 1 } else { 0 });
    }
}

#[test]
fn backed_preparation_panic_after_retention_keeps_whole_original_owner() {
    let f = Fixture::new(); let mut owner = f.staged(api::TestPoint::PanicAfterRetention);
    assert!(catch_unwind(AssertUnwindSafe(|| owner.run_backed())).is_err());
    assert!(owner.retention.is_some()); assert!(owner.publication().unwrap().acknowledgment().is_ok());
    assert_eq!(owner.core.sql, SqlOutcome::Begun); assert!(owner.core.key_locked);
    assert!(owner.acknowledgment().is_err()); assert!(owner.run_backed().is_err()); assert_eq!(f.count(), 0);
}

#[test]
fn backed_preparation_metadata_write_outcome_retains_staging_without_publication() {
    let f = Fixture::new(); let mut owner = f.staged(api::TestPoint::AfterMetadata);
    assert!(owner.run_backed().is_err()); let stage = owner.staging_id_data().unwrap();
    assert!(f.state().staging.contains_key(stage)); assert!(owner.publication().is_none());
    assert!(owner.core.store.as_ref().unwrap().gc.is_none()); assert!(owner.core.key_locked);
    assert!(owner.acknowledgment().is_err()); assert_eq!(f.count(), 0);
}

#[test]
fn backed_preparation_gc_without_actual_owner_refuses_before_any_deletion() {
    let f = Fixture::new(); let mut out = None; f.run(&mut out, None).unwrap();
    let garbage = f.index.store.put_blob(b"unrelated GC candidate").unwrap(); let mut gc = None;
    assert!(f.index.store.gc_backed_retaining(&mut gc, Vec::new(), Duration::ZERO, Clock::new()).is_err());
    assert!(gc.as_ref().unwrap().acknowledgment().is_err()); assert_eq!(gc.as_ref().unwrap().partial_report_data().unwrap().removed_objects, 0);
    assert!(f.index.store.object_path(&garbage).unwrap().exists()); assert!(out.as_ref().unwrap().acknowledgment().is_ok());
}

#[test]
fn backed_preparation_actual_owner_retains_complete_receipt_graph_through_gc() {
    let f = Fixture::new(); let mut out = None; f.run(&mut out, None).unwrap();
    let ack = out.as_ref().unwrap().acknowledgment().unwrap();
    let garbage = f.index.store.put_blob(b"unrelated removable object").unwrap(); let mut gc = None;
    f.index.store.gc_backed_retaining(&mut gc, vec![ack.retention().clone()], Duration::ZERO, Clock::new()).unwrap();
    assert!(gc.as_ref().unwrap().acknowledgment().unwrap().removed_objects >= 1);
    assert!(!f.index.store.object_path(&garbage).unwrap().exists());
    for reference in [&ack.prepared().key, &ack.prepared().root, &ack.prepared().receipt] {
        assert!(f.index.store.object_path(reference).unwrap().exists());
    }
    for reference in &f.anchors { assert!(f.index.store.object_path(reference).unwrap().exists()); }
    assert!(!f.index.store.object_path(&f.payload).unwrap().exists());
}

#[test]
fn backed_preparation_changed_network_input_misses_and_preserves_old_backed_owner() {
    let mut f = Fixture::new(); let mut old = None; f.run(&mut old, None).unwrap();
    let old_root = old.as_ref().unwrap().acknowledgment().unwrap().prepared().root.clone();
    let next = f.index.store.put_blob(b"new actual VM network input\n").unwrap(); f.index.store.pin("next-network", &next).unwrap();
    f.anchors[0] = next.clone(); f.spec.installer_inputs.insert("vm/network".into(), next);
    let mut current = None; f.run(&mut current, None).unwrap();
    assert_ne!(current.as_ref().unwrap().acknowledgment().unwrap().prepared().root, old_root);
    assert_eq!(f.count(), 2); assert_eq!(f.state().backed_retention.unwrap().pins.len(), 2);
    let mut read = None; old.as_ref().unwrap().acknowledgment().unwrap().retention()
        .read_retaining(&mut read, old_root, Clock::new()).unwrap(); assert!(read.as_ref().unwrap().verified_bytes().is_ok());
}

#[test]
fn backed_preparation_unretained_network_and_invalid_contracts_never_insert() {
    for variant in 0..6 {
        let mut f = Fixture::new(); let mut layout = PreparedContentLayout::PagedChunkedV2;
        match variant {
            0 => { f.index.store.unpin("actual-network").unwrap(); }
            1 => { f.spec.effects.insert(BuildEffect::Network); }
            2 => { f.spec.installer_commands.push(vec!["must-never-run".into()]); }
            3 => { layout = PreparedContentLayout::LegacyV1; }
            4 => { f.spec.platform.properties.insert("os".into(), "conflicting".into()); }
            _ => { f.spec.installer_inputs.insert("bad/action-key".into(), ObjectRef::from_bytes(ObjectKind::ActionKey, b"not-content")); }
        };
        let mut out = None;
        assert!(f.index.prepare_backed_retaining(&mut out, f.spec.clone(), layout, f.spec.base_runtime.clone(),
            f.anchors.clone(), vec![f.source.clone()], None, budget(), cancel()).is_err());
        assert!(out.as_ref().unwrap().acknowledgment().is_err()); assert_eq!(f.count(), 0);
    }
}

#[test]
fn backed_preparation_wrong_tool_and_wrong_paged_parent_keep_pin_or_unknown_hold_without_sql() {
    for wrong_parent in [false, true] {
        let mut f = Fixture::new();
        if wrong_parent {
            let stage = f.index.store.begin_staging(Duration::from_secs(30)).unwrap();
            let directory = LocalDirectoryStore::new(&f.index.store, &stage.id);
            f.spec.base_runtime = PagedDirectory::new(&directory).build(0o555, vec![DirectoryEntry::new(b"tool",
                EntryNode::File { object: f.spec.toolchains["compiler"].executable.clone(), logical_size: 1, mode: 0o555 }).unwrap()]).unwrap();
        } else { f.spec.toolchains.get_mut("compiler").unwrap().executable = f.anchors[0].clone(); }
        let mut out = None; assert!(f.run(&mut out, None).is_err()); let owner = out.as_ref().unwrap();
        assert!(owner.publication().is_some()); assert!(owner.acknowledgment().is_err()); assert_eq!(f.count(), 0);
        let state = f.state().backed_retention.unwrap();
        if wrong_parent { assert_eq!(state.holds.len(), 1); assert!(owner.publication().unwrap().acknowledgment().is_err()); }
        else { assert_eq!(state.pins.len(), 1); assert!(owner.publication().unwrap().acknowledgment().is_ok()); }
    }
}

#[test]
fn backed_preparation_actual_sqlite_busy_wait_has_one_original_clock() {
    // Allow ordinary fixture capture under parallel load before contention. The
    // same original deadline still bounds the actual blocked SQLite operation.
    let f = Fixture::new(); let blocker = Connection::open(f.index.root.join("index.sqlite")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let original = FinalizationBudget::begin(Duration::from_secs(2)).unwrap(); let constraint = original.constraint();
    let start = Instant::now(); let mut out = None;
    assert!(f.index.prepare_backed_retaining(&mut out, f.spec.clone(), PreparedContentLayout::PagedChunkedV2,
        f.spec.base_runtime.clone(), f.anchors.clone(), vec![f.source.clone()], None, original, cancel()).is_err());
    let owner = out.as_ref().unwrap(); assert_eq!(owner.original_constraint_data(), constraint);
    assert!(start.elapsed() < Duration::from_secs(5)); assert!(owner.core.connection.is_some());
    assert!(owner.core.store.is_none()); assert!(owner.acknowledgment().is_err()); blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn backed_preparation_original_keeper_mutex_and_gc_wait_remain_bounded() {
    // Encoding and capture share the original deadline with the blocked stage.
    // A 30ms deadline could expire before that stage on a loaded test host.
    for keeper in [false, true] {
        let f = Fixture::new(); let mut out = None; let original = FinalizationBudget::begin(Duration::from_secs(2)).unwrap();
        let constraint = original.constraint();
        let lock = OpenOptions::new().read(true).write(true).open(f.index.store.root().join("lock")).unwrap();
        let guard = if keeper { Some(f.index.connection.lock().unwrap()) } else { FileExt::lock_exclusive(&lock).unwrap(); None };
        let start = Instant::now();
        assert!(f.index.prepare_backed_retaining(&mut out, f.spec.clone(), PreparedContentLayout::PagedChunkedV2,
            f.spec.base_runtime.clone(), f.anchors.clone(), vec![f.source.clone()], None, original, cancel()).is_err());
        assert_eq!(out.as_ref().unwrap().original_constraint_data(), constraint);
        assert!(start.elapsed() < Duration::from_secs(5)); assert!(out.as_ref().unwrap().acknowledgment().is_err());
        if keeper { assert!(out.as_ref().unwrap().core.connection.is_none()); }
        else { assert!(out.as_ref().unwrap().core.store.as_ref().unwrap().gc.is_some()); }
        drop(guard);
    }
}

#[test]
fn backed_preparation_original_index_replacement_refuses_without_minting_owner() {
    let f = Fixture::new(); let replacement = f.temporary.path().join("replacement-index");
    drop(BuildIndex::open(&replacement, f.index.store.clone()).unwrap());
    let displaced = f.temporary.path().join("displaced-original-index");
    fs::rename(&f.index.root, &displaced).unwrap(); fs::rename(&replacement, &f.index.root).unwrap();
    let mut out = None; let result = f.run(&mut out, None);
    // Restore names before connection disposal; do not let an original SQLite
    // close touch an unrelated replacement's WAL/SHM names.
    fs::rename(&f.index.root, &replacement).unwrap(); fs::rename(&displaced, &f.index.root).unwrap();
    assert!(result.is_err());
    let owner = out.as_ref().unwrap(); assert!(owner.acknowledgment().is_err()); assert!(owner.core.connection.is_none()); assert!(owner.publication().is_none());
}

#[test]
fn backed_preparation_cancelled_input_stages_actual_owner_before_any_index_effect() {
    let f = Fixture::new(); let original = budget(); original.cancel(); let mut out = None;
    let weak = Arc::downgrade(&f.source);
    assert!(f.index.prepare_backed_retaining(&mut out, f.spec.clone(), PreparedContentLayout::PagedChunkedV2,
        f.spec.base_runtime.clone(), f.anchors.clone(), vec![f.source.clone()], None, original, cancel()).is_err());
    assert!(weak.strong_count() >= 2); assert!(out.as_ref().unwrap().core.key_file.is_none());
    assert!(out.as_ref().unwrap().acknowledgment().is_err()); assert_eq!(f.count(), 0);
}

#[path="environment_subgraph_tests.rs"]
mod environment_subgraph_tests;

#[path="canonical_retained_input_tests.rs"]
mod canonical_retained_input_tests;
