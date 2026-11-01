//! Real local SQLite/CAS/flock fixtures. No provider, native backend or compiler
//! invocation. Faults model a caller losing an outcome AFTER the real effect.
use super::*;
use crate::store::{DirectoryEntry, DirectoryManifest, EntryNode, RetentionState};
use std::{collections::BTreeSet, thread, time::Instant};

struct Fixture { index: Arc<BuildIndex>, spec: PreparationSpec, directory: ObjectRef,
    anchors: Vec<ObjectRef>, _temporary: tempfile::TempDir }
impl Fixture {
    fn new() -> Self {
        let temporary = if Path::new("/workspace").is_dir() { tempfile::tempdir_in("/workspace").unwrap() } else { tempfile::tempdir().unwrap() };
        let store = LocalStore::open(temporary.path().join("cas")).unwrap();
        let binary = store.put_blob(b"immutable compiler fixture\n").unwrap();
        let directory = store.put_directory(DirectoryManifest { format_version: 1, mode: 0o555,
            entries: vec![DirectoryEntry::new(b"compiler", EntryNode::File { object: binary.clone(), mode: 0o555 }).unwrap()] }).unwrap();
        let model = store.put_directory(DirectoryManifest { format_version: 1, mode: 0o555, entries: vec![] }).unwrap();
        let network = store.put_blob(b"actual original VM network fixture\n").unwrap();
        for (name, root) in [("original-compiler", &directory), ("original-model-image", &model), ("original-network", &network)] { store.pin(name, root).unwrap(); }
        let anchors = vec![directory.clone(), model.clone(), network.clone()];
        let spec = PreparationSpec { format_version: 1,
            platform: PlatformIdentity { os: "linux".into(), architecture: "x86_64".into(), abi: "gnu".into(), properties: BTreeMap::from([("runtime_profile".into(), "fixture-v1".into())]) },
            base_runtime: directory.clone(),
            installer_inputs: BTreeMap::from([("model/full-image".into(), model), ("vm/network".into(), network)]),
            installer_commands: vec![], installer_environment: BTreeMap::new(),
            toolchains: BTreeMap::from([("compiler".into(), ToolchainIdentity { version: "fixture-1".into(), closure: directory.clone(),
                executable: binary, executable_path: "compiler".into(), configuration: BTreeMap::new() })]),
            policy_version: "retained-fixture-v1".into(), effects: BTreeSet::new() };
        let index = Arc::new(BuildIndex::open(temporary.path().join("index"), store).unwrap());
        Self { index, spec, directory, anchors, _temporary: temporary }
    }
    fn run(&self, out: &mut Option<RetainedPreparation>, layout: PreparedContentLayout,
        original: FinalizationBudget, cancelled: Arc<AtomicBool>) -> Result<()> {
        self.index.prepare_pinned_retaining(out, self.spec.clone(), layout, self.directory.clone(), self.anchors.clone(), original, cancelled)
    }
    fn owner(&self, fault: TestPoint) -> RetainedPreparation {
        let mut owner = RetainedPreparation::stage(self.index.clone(), self.spec.clone(), PreparedContentLayout::PagedChunkedV2,
            self.directory.clone(), self.anchors.clone(), budget(), cancelled());
        owner.fault = Some(fault); owner
    }
    fn count(&self) -> i64 {
        let connection = Connection::open(self.index.root.join("index.sqlite")).unwrap();
        connection.query_row("SELECT count(*) FROM build_index", [], |r| r.get(0)).unwrap()
    }
    // Read-only observation under the original retained flock. This cannot
    // recreate a success owner or bypass production retention admission.
    fn raw_retention(&self) -> RetentionState {
        let value: serde_json::Value = serde_json::from_slice(&fs::read(self.index.store.root().join("retention.json")).unwrap()).unwrap();
        serde_json::from_value(value["state"].clone()).unwrap()
    }
}
fn budget() -> FinalizationBudget { FinalizationBudget::begin(Duration::from_secs(5)).unwrap() }
fn cancelled() -> Arc<AtomicBool> { Arc::new(AtomicBool::new(false)) }
fn expected_pin(owner: &RetainedPreparation) -> String { pin(owner.layout.category(), &owner.expected.as_ref().unwrap().key) }

#[test]
fn retained_preparation_uses_one_canonical_root_through_an_ancestor_alias() {
    let f = Fixture::new();
    let alias = f._temporary.path().join("index-parent-alias");
    std::os::unix::fs::symlink(f.index.root.parent().unwrap(), &alias).unwrap();
    let aliased = Arc::new(BuildIndex::open(alias.join("index"), f.index.store.clone()).unwrap());
    assert_eq!(aliased.root, f.index.root);
    assert_eq!(aliased.locks, f.index.root.join("locks"));
    assert_eq!(aliased.locks.canonicalize().unwrap(), aliased.locks);
    let mut out = None;
    aliased.prepare_pinned_retaining(&mut out, f.spec.clone(), PreparedContentLayout::PagedChunkedV2,
        f.directory.clone(), f.anchors.clone(), budget(), cancelled()).unwrap();
    assert!(out.as_ref().unwrap().acknowledgment().is_ok());
    aliased.validate_prepared(out.as_ref().unwrap().acknowledgment().unwrap().prepared()).unwrap();
}

#[test]
fn retained_preparation_still_refuses_a_redirected_lock_directory() {
    let f = Fixture::new();
    let displaced = f.index.root.join("original-locks");
    fs::rename(&f.index.locks, &displaced).unwrap();
    std::os::unix::fs::symlink(&displaced, &f.index.locks).unwrap();
    let mut out = None;
    assert!(f.run(&mut out, PreparedContentLayout::PagedChunkedV2, budget(), cancelled()).is_err());
    let owner = out.as_ref().unwrap();
    assert!(owner.failure().unwrap().to_string().contains("redirects"));
    assert!(owner.key_file.is_none() && owner.connection.is_none() && owner.store.is_none());
    assert!(owner.acknowledgment().is_err());
    assert_eq!(f.count(), 0);
}

#[test]
fn retained_preparation_real_cas_sql_ack_preserves_both_layouts_and_installer_inputs() {
    let f = Fixture::new(); let mut results = Vec::new();
    for layout in [PreparedContentLayout::LegacyV1, PreparedContentLayout::PagedChunkedV2] {
        let original = budget(); let constraint = original.constraint(); let mut out = None;
        f.run(&mut out, layout, original, cancelled()).unwrap();
        let owner = out.as_ref().unwrap(); let ack = owner.acknowledgment().unwrap();
        assert_eq!(owner.original_constraint_data(), constraint); assert!(!ack.reused());
        assert_eq!(owner.sql, SqlOutcome::Committed); assert!(!owner.key_locked);
        assert!(owner.store.as_ref().unwrap().gc.is_none());
        let prepared = ack.prepared().clone();
        assert_eq!(prepared.key, f.spec.key_with_layout(&f.index.store, layout).unwrap());
        f.index.validate_prepared(&prepared).unwrap();
        assert_eq!(f.index.store.retention().unwrap().pins[&expected_pin(owner)], prepared.receipt);
        let bytes = f.index.store.read_object(&prepared.root).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let inputs: Vec<ObjectRef> = serde_json::from_value(value["installer_inputs"].clone()).unwrap();
        for r in f.spec.installer_inputs.values() { assert!(inputs.contains(r)); }
        assert_eq!(value["platform"]["runtime_profile"], "fixture-v1");
        let mut repeated = None; f.run(&mut repeated, layout, budget(), cancelled()).unwrap();
        assert!(repeated.as_ref().unwrap().acknowledgment().unwrap().reused());
        assert_eq!(repeated.as_ref().unwrap().acknowledgment().unwrap().prepared(), &prepared);
        assert!(repeated.as_ref().unwrap().staging_id_data().is_none());
        results.push(prepared);
    }
    assert_ne!(results[0].key, results[1].key); assert_ne!(results[0].root, results[1].root);
    assert_eq!(f.count(), 2);
}

#[test]
fn retained_preparation_real_key_flock_release_uses_the_original_budget() {
    let f = Fixture::new(); let layout = PreparedContentLayout::PagedChunkedV2;
    let key = f.spec.key_with_layout(&f.index.store, layout).unwrap();
    let locked = f.index.key_lock(layout.category(), &key).unwrap();
    let releaser = thread::spawn(move || { thread::sleep(Duration::from_millis(20)); drop(locked); });
    let original = budget(); let before = original.constraint(); let mut out = None;
    f.run(&mut out, layout, original, cancelled()).unwrap(); releaser.join().unwrap();
    assert_eq!(out.as_ref().unwrap().original_constraint_data(), before);
    assert!(out.as_ref().unwrap().acknowledgment().is_ok());
}

#[test]
fn retained_preparation_key_wait_expiry_and_cancellation_keep_the_actual_carrier() {
    for cancel_wait in [false, true] {
        let f = Fixture::new(); let layout = PreparedContentLayout::PagedChunkedV2;
        let key = f.spec.key_with_layout(&f.index.store, layout).unwrap();
        let locked = f.index.key_lock(layout.category(), &key).unwrap();
        let original = FinalizationBudget::begin(Duration::from_millis(if cancel_wait { 1000 } else { 50 })).unwrap();
        let constraint = original.constraint(); let stop = cancelled(); let copy = stop.clone();
        let signal = thread::spawn(move || { if cancel_wait { thread::sleep(Duration::from_millis(10)); copy.store(true, Ordering::Release); } });
        let mut out = None; let started = Instant::now();
        assert!(f.run(&mut out, layout, original.clone(), stop).is_err()); signal.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        let owner = out.as_ref().unwrap(); assert_eq!(owner.original_constraint_data(), constraint);
        assert_eq!(owner.status(), RetainedPreparationStatus::Failed); assert!(owner.key_file.is_some());
        assert!(!owner.key_locked); assert!(owner.connection.is_none()); assert!(owner.acknowledgment().is_err());
        assert!(original.remaining().is_err()); assert_eq!(f.count(), 0); drop(locked);
    }
}

#[test]
fn retained_preparation_real_sqlite_writer_contention_is_bounded_and_can_settle() {
    let f = Fixture::new();
    let blocker = Connection::open(f.index.root.join("index.sqlite")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let release = thread::spawn(move || { thread::sleep(Duration::from_millis(20)); blocker.execute_batch("ROLLBACK").unwrap(); });
    let mut out = None; let original = budget(); let before = original.constraint();
    f.run(&mut out, PreparedContentLayout::PagedChunkedV2, original, cancelled()).unwrap(); release.join().unwrap();
    assert_eq!(out.as_ref().unwrap().original_constraint_data(), before); assert_eq!(f.count(), 1);
    assert!(out.as_ref().unwrap().acknowledgment().is_ok());
}

#[test]
fn retained_preparation_sqlite_busy_expiry_never_gets_a_new_write_budget() {
    let f = Fixture::new(); let blocker = Connection::open(f.index.root.join("index.sqlite")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let original = FinalizationBudget::begin(Duration::from_millis(50)).unwrap(); let before = original.constraint(); let mut out = None;
    assert!(f.run(&mut out, PreparedContentLayout::PagedChunkedV2, original.clone(), cancelled()).is_err());
    let owner = out.as_ref().unwrap(); assert_eq!(owner.original_constraint_data(), before);
    assert_eq!(owner.sql, SqlOutcome::BeginAttempted); assert!(owner.store.is_none());
    assert!(owner.connection.as_ref().unwrap().is_autocommit()); assert!(owner.key_locked);
    assert!(owner.acknowledgment().is_err()); assert!(original.remaining().is_err());
    blocker.execute_batch("ROLLBACK").unwrap(); assert_eq!(f.count(), 0);
}

#[test]
fn retained_preparation_actual_gc_flock_wait_has_no_hidden_blocking_admission() {
    let f = Fixture::new(); let path = f.index.store.root().join("lock");
    let blocker = OpenOptions::new().read(true).write(true).open(path).unwrap(); FileExt::lock_exclusive(&blocker).unwrap();
    let original = FinalizationBudget::begin(Duration::from_millis(50)).unwrap(); let mut out = None;
    assert!(f.run(&mut out, PreparedContentLayout::PagedChunkedV2, original, cancelled()).is_err());
    let owner = out.as_ref().unwrap(); let store = owner.store.as_ref().unwrap();
    assert!(store.gc.is_some()); assert!(!store.gc_acquired); assert!(store.lease.is_none());
    assert_eq!(owner.sql, SqlOutcome::Begun); assert!(!owner.connection.as_ref().unwrap().is_autocommit());
    assert!(owner.acknowledgment().is_err()); drop(blocker);
}

#[test]
fn retained_preparation_unretained_or_wrong_tool_cannot_publish() {
    for wrong_tool in [false, true] {
        let f = Fixture::new(); let mut spec = f.spec.clone(); let mut anchors = f.anchors.clone();
        if wrong_tool { spec.toolchains.get_mut("compiler").unwrap().executable = f.spec.installer_inputs["vm/network"].clone(); }
        else { anchors.retain(|r| r != &f.directory); }
        let mut out = None;
        assert!(f.index.prepare_pinned_retaining(&mut out, spec, PreparedContentLayout::PagedChunkedV2,
            f.directory.clone(), anchors, budget(), cancelled()).is_err());
        let owner = out.as_ref().unwrap(); assert!(owner.acknowledgment().is_err());
        assert!(owner.staging_id_data().is_none()); assert_eq!(f.count(), 0);
    }
}

#[test]
fn retained_preparation_staging_failure_keeps_real_intent_and_never_retries() {
    let f = Fixture::new(); let mut owner = f.owner(TestPoint::AfterStaging);
    assert!(owner.run().is_err()); let original = owner.original_constraint_data();
    let id = owner.staging_id_data().unwrap().to_owned(); assert!(f.raw_retention().staging.contains_key(&id));
    assert!(owner.store.as_ref().unwrap().gc_acquired); assert!(owner.key_locked);
    assert_eq!(owner.sql, SqlOutcome::Begun); assert!(owner.acknowledgment().is_err());
    assert!(owner.run().is_err()); assert_eq!(owner.original_constraint_data(), original); assert_eq!(f.count(), 0);
}

#[test]
fn retained_preparation_partial_pin_directory_sync_keeps_original_fd_and_unknown_effect() {
    let f = Fixture::new(); let mut owner = f.owner(TestPoint::PinDirectorySync);
    assert!(owner.run().is_err()); assert!(owner.acknowledgment().is_err());
    let store = owner.store.as_ref().unwrap();
    assert!(store.gc_acquired && store.pending_file.is_some() && store.pending_path.is_some());
    assert!(store.retention_intent.is_some() && store.pin_intent.is_some());
    // rename really happened, but the original directory sync/ACK did not.
    assert_eq!(f.raw_retention().pins[&expected_pin(&owner)], owner.expected.as_ref().unwrap().receipt);
    assert!(owner.pin_generation.is_none()); assert_eq!(f.count(), 0); assert!(owner.run().is_err());
}

#[test]
fn retained_preparation_real_sqlite_insert_failure_does_not_discard_the_real_pin() {
    let f = Fixture::new(); let setup = Connection::open(f.index.root.join("index.sqlite")).unwrap();
    setup.execute_batch("CREATE TRIGGER refuse_insert BEFORE INSERT ON build_index BEGIN SELECT RAISE(ABORT,'fixture-insert-failed'); END;").unwrap();
    let mut out = None; assert!(f.run(&mut out, PreparedContentLayout::PagedChunkedV2, budget(), cancelled()).is_err());
    let owner = out.as_ref().unwrap(); assert_eq!(owner.sql, SqlOutcome::InsertAttempted);
    assert!(owner.pin_generation.is_some()); assert!(!owner.connection.as_ref().unwrap().is_autocommit());
    assert_eq!(f.raw_retention().pins[&expected_pin(owner)], owner.expected.as_ref().unwrap().receipt);
    assert!(owner.store.as_ref().unwrap().gc_acquired); assert!(owner.acknowledgment().is_err()); assert_eq!(f.count(), 0);
}

#[test]
fn retained_preparation_known_commit_lost_ack_is_not_reconstructed_from_readback() {
    let f = Fixture::new(); let mut owner = f.owner(TestPoint::AfterCommit);
    assert!(owner.run().is_err()); assert_eq!(owner.sql, SqlOutcome::Committed);
    assert_eq!(f.count(), 1); assert!(owner.pin_generation.is_some());
    assert_eq!(f.raw_retention().pins[&expected_pin(&owner)], owner.expected.as_ref().unwrap().receipt);
    assert!(owner.store.as_ref().unwrap().gc_acquired); assert!(owner.key_locked);
    assert!(owner.acknowledgment().is_err()); assert!(owner.run().is_err());
}

#[test]
fn retained_preparation_expired_input_still_has_a_carrier_and_never_installs_or_resets() {
    let f = Fixture::new(); let original = budget(); original.cancel(); let mut out = None;
    assert!(f.run(&mut out, PreparedContentLayout::PagedChunkedV2, original, cancelled()).is_err());
    assert!(out.is_some()); assert!(out.as_ref().unwrap().key_file.is_none());
    let mut spec = f.spec.clone(); spec.installer_commands.push(vec!["forbidden-runtime-install".into()]); let mut installer = None;
    assert!(f.index.prepare_pinned_retaining(&mut installer, spec, PreparedContentLayout::PagedChunkedV2,
        f.directory.clone(), f.anchors.clone(), budget(), cancelled()).is_err());
    assert!(installer.as_ref().unwrap().connection.is_none()); assert_eq!(f.count(), 0);
}

#[test]
fn retained_preparation_paged_tool_file_nodes_and_actual_summaries_are_checked() {
    use crate::{content::{ContentStore, FileNode, FILE_LAYOUT}, directory::{DirectoryEntry as PagedEntry, EntryNode as PagedNode, LocalDirectoryStore, PagedDirectory}};
    let mut f = Fixture::new(); let store = &f.index.store;
    let staging = store.begin_staging(Duration::from_secs(30)).unwrap();
    let staged = crate::content::StagedContentStore::new(store, &staging.id).unwrap();
    let payload = ContentStore::put_blob(&staged, &vec![b'X'; 4 * 1024 * 1024]).unwrap();
    let file = ContentStore::put_file_node(&staged, &FileNode { format_version: 1, layout: FILE_LAYOUT.into(), level: 0,
        logical_size: payload.size_bytes, children: vec![payload] }).unwrap();
    let adapter = LocalDirectoryStore::new(store, &staging.id);
    let root = PagedDirectory::new(&adapter).build(0o555,
        vec![PagedEntry::new(b"compiler", PagedNode::File { object: file.clone(), logical_size: 4 * 1024 * 1024, mode: 0o555 }).unwrap()]).unwrap();
    store.publish(&staging.id, "actual-paged-compiler", &root).unwrap();
    f.directory = root.clone(); f.spec.base_runtime = root.clone(); f.anchors.push(root.clone());
    f.spec.toolchains.get_mut("compiler").unwrap().closure = root;
    f.spec.toolchains.get_mut("compiler").unwrap().executable = file;
    let mut out = None; f.run(&mut out, PreparedContentLayout::PagedChunkedV2, budget(), cancelled()).unwrap();
    let prepared = out.as_ref().unwrap().acknowledgment().unwrap().prepared();
    f.index.validate_prepared(prepared).unwrap();
}

#[test]
fn retained_preparation_pin_and_insert_outcomes_stay_original_without_implicit_rollback() {
    for point in [TestPoint::AfterPin, TestPoint::AfterInsert] {
        let f = Fixture::new(); let mut owner = f.owner(point); assert!(owner.run().is_err());
        assert!(owner.pin_generation.is_some()); assert_eq!(f.count(), 0);
        assert_eq!(f.raw_retention().pins[&expected_pin(&owner)], owner.expected.as_ref().unwrap().receipt);
        assert!(!owner.connection.as_ref().unwrap().is_autocommit());
        assert_eq!(owner.sql, if point == TestPoint::AfterPin { SqlOutcome::Begun } else { SqlOutcome::Inserted });
        assert!(owner.store.as_ref().unwrap().gc_acquired && owner.key_locked);
        assert!(owner.acknowledgment().is_err()); assert!(owner.run().is_err());
    }
}

#[test]
fn retained_preparation_late_actual_commit_cannot_mint_a_timely_ack() {
    let f = Fixture::new(); let mut owner = f.owner(TestPoint::CancelAfterCommit);
    let original = owner.original_constraint_data(); assert!(owner.run().is_err());
    assert_eq!(owner.sql, SqlOutcome::Committed); assert_eq!(f.count(), 1);
    assert!(owner.pin_generation.is_some()); assert!(owner.original.remaining().is_err());
    assert_eq!(owner.original_constraint_data(), original); assert!(owner.acknowledgment().is_err());
    assert!(owner.store.as_ref().unwrap().gc_acquired && owner.key_locked);
}

#[test]
fn retained_preparation_cancel_after_real_staging_keeps_the_same_bounded_lease() {
    let f = Fixture::new(); let mut owner = f.owner(TestPoint::CancelAfterStaging);
    let original = owner.original_constraint_data(); assert!(owner.run().is_err());
    let lease = owner.store.as_ref().unwrap().lease.as_ref().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    assert!(lease.expires_at_ms <= now + 5000); // Same original5s, never3600s.
    assert_eq!(f.raw_retention().staging[&lease.id], *lease);
    assert!(owner.store.as_ref().unwrap().gc_acquired); assert_eq!(owner.object_attempts, 0);
    assert_eq!(owner.original_constraint_data(), original); assert!(owner.original.remaining().is_err());
    assert!(owner.acknowledgment().is_err()); assert_eq!(f.count(), 0);
}

#[test]
fn retained_preparation_unindexed_durable_pin_is_not_adopted_as_a_readback_ack() {
    let f = Fixture::new(); let mut interrupted = f.owner(TestPoint::AfterPin);
    assert!(interrupted.run().is_err()); let original = interrupted.original.clone();
    let pin_name = expected_pin(&interrupted); let reference = interrupted.expected.as_ref().unwrap().receipt.clone();
    // Deliberate fixture loss, not production cleanup/recovery. Only the real
    // durable pin survives; it must not reconstruct the lost original owner.
    drop(interrupted); assert_eq!(f.count(), 0);
    let mut out = None; assert!(f.run(&mut out, PreparedContentLayout::PagedChunkedV2, original, cancelled()).is_err());
    let owner = out.as_ref().unwrap(); assert!(owner.acknowledgment().is_err());
    assert!(owner.staging_id_data().is_none()); assert_eq!(owner.object_attempts, 0);
    assert_eq!(f.raw_retention().pins[&pin_name], reference); assert_eq!(f.count(), 0);
}

#[test]
fn retained_preparation_same_size_corrupt_descriptor_never_becomes_an_index_hit() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(); let mut first = None;
    f.run(&mut first, PreparedContentLayout::PagedChunkedV2, budget(), cancelled()).unwrap();
    let receipt = first.as_ref().unwrap().acknowledgment().unwrap().prepared().receipt.clone();
    let path = f.index.store.object_path(&receipt).unwrap(); let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 1; fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(); fs::write(&path, &bytes).unwrap();
    let mut repeated = None; assert!(f.run(&mut repeated, PreparedContentLayout::PagedChunkedV2, budget(), cancelled()).is_err());
    assert!(repeated.as_ref().unwrap().acknowledgment().is_err()); assert_eq!(f.count(), 1);
}

#[test]
fn retained_preparation_preserves_real_v2_object_set_ledger_and_existing_root_pins() {
    let f = Fixture::new(); let store = &f.index.store;
    let stage = store.begin_staging(Duration::from_secs(30)).unwrap();
    let network = f.spec.installer_inputs["vm/network"].clone();
    let set = store.retain_object_set(&stage.id, "independent-explicit-set", &[network.clone()]).unwrap();
    assert_eq!(store.retention().unwrap().format_version, 2);
    let mut out = None; f.run(&mut out, PreparedContentLayout::PagedChunkedV2, budget(), cancelled()).unwrap();
    assert!(out.as_ref().unwrap().acknowledgment().is_ok());
    let retained = store.retention().unwrap(); assert_eq!(retained.format_version, 2);
    assert_eq!(retained.pins["original-network"], network);
    assert_eq!(store.object_set_refs(&set).unwrap(), vec![network]);
}

#[test]
fn retained_preparation_refuses_a_replacement_before_any_preparation_effect() {
    let f = Fixture::new();
    let replacement = f._temporary.path().join("replacement-index");
    // A fully valid second index is still not the ORIGINAL SQLite keeper.
    drop(BuildIndex::open(&replacement, f.index.store.clone()).unwrap());
    let displaced = f._temporary.path().join("displaced-original-index");
    fs::rename(&f.index.root, &displaced).unwrap();
    fs::rename(&replacement, &f.index.root).unwrap();
    let original = budget(); let constraint = original.constraint(); let mut out = None;
    let result = f.run(&mut out, PreparedContentLayout::PagedChunkedV2, original, cancelled());
    // Restore before assertions/disposal: never let an old SQLite close touch
    // an unrelated replacement index's sidecar names.
    fs::rename(&f.index.root, &replacement).unwrap();
    fs::rename(&displaced, &f.index.root).unwrap();
    assert!(result.is_err());
    let owner = out.as_ref().unwrap();
    assert!(Arc::ptr_eq(&owner.index, &f.index));
    assert_eq!(owner.original_constraint_data(), constraint);
    assert!(owner.acknowledgment().is_err());
    assert!(owner.expected.is_none() && owner.key_file.is_none());
    assert!(owner.connection.is_none() && owner.store.is_none());
    assert!(owner.failure().unwrap().to_string().contains("moved"));
    assert_eq!(f.count(), 0);
    let replacement_connection = Connection::open(replacement.join("index.sqlite")).unwrap();
    assert_eq!(replacement_connection.query_row("SELECT count(*) FROM build_index", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(f.raw_retention().pins.len(), 3);
}

#[test]
fn retained_preparation_checks_the_actual_dedicated_connection_after_a_swap() {
    let f = Fixture::new();
    let mut owner = RetainedPreparation::stage(f.index.clone(), f.spec.clone(), PreparedContentLayout::PagedChunkedV2,
        f.directory.clone(), f.anchors.clone(), budget(), cancelled());
    owner.encode().unwrap(); owner.capture_index().unwrap();
    let original = f.index.root.join("index.sqlite");
    let displaced = f.index.root.join("original.sqlite");
    let foreign = f._temporary.path().join("foreign.sqlite");
    // The foreign DB is closed and has no WAL sidecars. Open it at the index
    // name without issuing SQL, then restore the original name. This models
    // a replacement between a connection's pre-open and post-open checks.
    let foreign_connection = Connection::open(&foreign).unwrap();
    foreign_connection.execute_batch("CREATE TABLE foreign_data(value INTEGER)").unwrap();
    drop(foreign_connection);
    fs::rename(&original, &displaced).unwrap(); fs::rename(&foreign, &original).unwrap();
    owner.sql = SqlOutcome::OpenAttempted;
    let opened = Connection::open_with_flags(&original, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
        | rusqlite::OpenFlags::SQLITE_OPEN_PRIVATE_CACHE);
    let opened_current = opened.as_ref().ok().map(sqlite_not_moved);
    // Put the foreign inode back at the name where its connection will fail
    // HAS_MOVED. No query opens an unrelated WAL/SHM during the swap.
    fs::rename(&original, &foreign).unwrap(); fs::rename(&displaced, &original).unwrap();
    opened_current.unwrap().unwrap();
    owner.connection = Some(opened.unwrap()); owner.sql = SqlOutcome::Opened;
    { let keeper = f.index.connection.lock().unwrap(); sqlite_binding_current(&keeper, &original).unwrap(); }
    assert!(owner.check().unwrap_err().to_string().contains("moved"));
    assert!(owner.connection.is_some()); assert!(owner.store.is_none());
    assert!(owner.acknowledgment().is_err()); assert_eq!(f.count(), 0);
}

#[test]
fn retained_preparation_original_keeper_wait_uses_the_same_budget_and_cancel() {
    for cancel_wait in [false, true] {
        let f = Fixture::new(); let keeper = f.index.connection.lock().unwrap();
        let original = FinalizationBudget::begin(Duration::from_millis(if cancel_wait { 1000 } else { 50 })).unwrap();
        let constraint = original.constraint(); let stop = cancelled(); let signal_stop = stop.clone();
        let signal = thread::spawn(move || {
            if cancel_wait { thread::sleep(Duration::from_millis(10)); signal_stop.store(true, Ordering::Release); }
        });
        let mut out = None; let started = Instant::now();
        assert!(f.run(&mut out, PreparedContentLayout::PagedChunkedV2, original.clone(), stop).is_err());
        signal.join().unwrap(); drop(keeper);
        assert!(started.elapsed() < Duration::from_secs(2));
        let owner = out.as_ref().unwrap(); assert_eq!(owner.original_constraint_data(), constraint);
        assert!(owner.witness.root.is_none() && owner.expected.is_none());
        assert!(owner.connection.is_none() && owner.store.is_none());
        assert!(Arc::ptr_eq(&owner.index, &f.index));
        assert!(owner.acknowledgment().is_err() && original.remaining().is_err());
        assert_eq!(f.count(), 0);
    }
}

#[test]
fn retained_preparation_original_keeper_can_become_available_without_a_new_budget() {
    let f = Fixture::new(); let index = f.index.clone();
    let (ready, waiting) = std::sync::mpsc::sync_channel(1);
    let blocker = thread::spawn(move || {
        let keeper = index.connection.lock().unwrap(); ready.send(()).unwrap();
        thread::sleep(Duration::from_millis(20)); drop(keeper);
    });
    waiting.recv().unwrap();
    let original = budget(); let constraint = original.constraint(); let mut out = None;
    f.run(&mut out, PreparedContentLayout::PagedChunkedV2, original, cancelled()).unwrap();
    blocker.join().unwrap();
    assert_eq!(out.as_ref().unwrap().original_constraint_data(), constraint);
    assert!(out.as_ref().unwrap().acknowledgment().is_ok()); assert_eq!(f.count(), 1);
}

fn sqlite_shared_range_is_held(path: &Path) -> bool {
    let helper = concat!(module_path!(), "::retained_preparation_posix_lock_probe_process");
    let helper = helper.split_once("::").unwrap().1; // libtest omits the crate name.
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", helper, "--nocapture", "--test-threads=1"])
        .env("RUDDER_PREPARATION_LOCK_PROBE_PATH", path).output().unwrap();
    assert!(result.status.success(), "isolated POSIX probe failed: {}", String::from_utf8_lossy(&result.stderr));
    let output = String::from_utf8(result.stdout).unwrap();
    assert!(output.contains("RUDDER_PREPARATION_LOCK_PROBE=held") || output.contains("RUDDER_PREPARATION_LOCK_PROBE=free"), "probe did not run");
    output.contains("RUDDER_PREPARATION_LOCK_PROBE=held")
}

#[test]
fn retained_preparation_successful_carrier_disposal_preserves_another_sqlite_lock() {
    let f = Fixture::new(); let mut out = None;
    f.run(&mut out, PreparedContentLayout::PagedChunkedV2, budget(), cancelled()).unwrap();
    let keeper = f.index.connection.lock().unwrap();
    keeper.execute_batch("BEGIN; SELECT count(*) FROM build_index").unwrap();
    let control = f._temporary.path().join("unlocked-probe-control"); fs::write(&control, b"").unwrap();
    assert!(!sqlite_shared_range_is_held(&control));
    let path = f.index.root.join("index.sqlite");
    assert!(sqlite_shared_range_is_held(&path));
    // V1 closed a raw File witness here and silently released EVERY same-process
    // POSIX main-DB lock. SQLite-owned closes must instead defer that FD close.
    drop(out.take());
    assert!(sqlite_shared_range_is_held(&path));
    keeper.execute_batch("ROLLBACK").unwrap();
}

#[test]
#[ignore = "subprocess-only nonblocking POSIX lock probe; invoked by the actual SQLite regression"]
fn retained_preparation_posix_lock_probe_process() {
    use std::os::fd::AsRawFd;
    let path = std::env::var_os("RUDDER_PREPARATION_LOCK_PROBE_PATH").expect("probe path absent");
    // ONLY this isolated process opens a raw DB FD; it has no SQLite connection
    // or parent locks to disturb. No raw DB/WAL/SHM File exists in production.
    let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW).open(path).unwrap();
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _; lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = 0x4000_0002; lock.l_len = 510; // SQLite's SHARED_FIRST / SHARED_SIZE.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock as *const libc::flock) };
    if result == 0 { println!("RUDDER_PREPARATION_LOCK_PROBE=free"); }
    else {
        let error = std::io::Error::last_os_error();
        assert!(matches!(error.raw_os_error(), Some(libc::EACCES | libc::EAGAIN)), "unexpected probe result: {error}");
        println!("RUDDER_PREPARATION_LOCK_PROBE=held");
    }
}
