use rudder_engine::{
    protocol::{canonical_json, ObjectKind, ObjectRef},
    store::{
        DirectoryEntry, DirectoryManifest, EntryNode, LocalStore, ObjectSetReceipt, RetentionState,
    },
};
use std::{
    fs,
    process::{Command, Stdio},
    time::Duration,
};

fn fixture() -> (tempfile::TempDir, LocalStore) {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    (temp, store)
}
fn lease(store: &LocalStore) -> String {
    store.begin_staging(Duration::from_secs(60)).unwrap().id
}

#[test]
fn explicit_metadata_survives_gc_without_granting_complete_root_authority() {
    let (_temp, store) = fixture();
    let stage = lease(&store);
    let remote = ObjectRef::from_bytes(ObjectKind::Blob, b"remote-only bytes");
    let directory = store
        .put_directory(DirectoryManifest {
            format_version: 1,
            mode: 0o755,
            entries: vec![DirectoryEntry::new(
                b"remote",
                EntryNode::File {
                    object: remote.clone(),
                    mode: 0o644,
                },
            )
            .unwrap()],
        })
        .unwrap();
    let orphan = store.put_blob(b"unreferenced local bytes").unwrap();
    let receipt = store
        .retain_object_set(&stage, "partial-directory", &[directory.clone()])
        .unwrap();
    assert_eq!(store.retention().unwrap().format_version, 2);
    assert!(store.retention().unwrap().pins.is_empty());
    assert!(store.pin_existing("pretend-complete", &directory).is_err());
    assert!(store.retained_closure(&directory).is_err());
    let complete_stage = lease(&store);
    assert!(store
        .publish(&complete_stage, "pretend-complete", &directory)
        .is_err());
    store.abandon_staging(&complete_stage).unwrap();
    assert_eq!(
        store.object_set_refs(&receipt).unwrap(),
        vec![directory.clone()]
    );
    let gc = store.gc(Duration::ZERO).unwrap();
    assert!(!gc.blocked_by_staging);
    assert!(store.read_object(&directory).is_ok());
    assert!(store.read_object(&remote).is_err());
    assert!(store.read_object(&orphan).is_err());
    let reopened = LocalStore::open(store.root()).unwrap();
    assert_eq!(reopened.object_set_refs(&receipt).unwrap(), vec![directory]);
}

#[test]
fn retries_bind_original_stage_and_stale_release_cannot_remove_replacement() {
    let (_temp, store) = fixture();
    let stage = lease(&store);
    let object = store.put_blob(b"retained").unwrap();
    let first = store
        .retain_object_set(&stage, "named", &[object.clone()])
        .unwrap();
    assert_eq!(
        store
            .retain_object_set(&stage, "named", &[object.clone(), object.clone()])
            .unwrap(),
        first
    );
    let other_stage = lease(&store);
    assert!(store
        .retain_object_set(&other_stage, "named", &[object.clone()])
        .is_err());
    assert!(store
        .retention()
        .unwrap()
        .staging
        .contains_key(&other_stage));
    let changed = store.put_blob(b"changed").unwrap();
    assert!(store
        .retain_object_set(&stage, "named", &[changed])
        .is_err());
    store.release_object_set(&first).unwrap();
    let next = store
        .retain_object_set(&other_stage, "named", &[object.clone()])
        .unwrap();
    assert!(next.generation > first.generation);
    assert!(store.release_object_set(&first).is_err());
    assert_eq!(store.object_set_refs(&next).unwrap(), vec![object.clone()]);
    store.release_object_set(&next).unwrap();
    store.release_object_set(&next).unwrap();
    store.gc(Duration::ZERO).unwrap();
    assert!(store.read_object(&object).is_err());
}

#[test]
fn interrupted_staging_cleanup_recovers_same_durable_receipt() {
    let (_temp, store) = fixture();
    let stage = lease(&store);
    let mut before = store.retention().unwrap();
    let object = store.put_blob(b"durable before acknowledgment").unwrap();
    let receipt = store
        .retain_object_set(&stage, "recover", &[object.clone()])
        .unwrap();
    // Reconstitute the exact conservative crash window: SQLite committed the
    // set, but retention.json still contains the original staging lease.
    before.format_version = 2;
    before.generation += 1;
    let checksum =
        ObjectRef::from_bytes(ObjectKind::Blob, &canonical_json(&before).unwrap()).digest;
    fs::write(
        store.root().join("retention.json"),
        canonical_json(&serde_json::json!({
            "state": before, "checksum": checksum,
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(store.gc(Duration::ZERO).unwrap().blocked_by_staging);
    assert_eq!(
        store
            .retain_object_set(&stage, "recover", &[object.clone()])
            .unwrap(),
        receipt
    );
    assert!(!store.retention().unwrap().staging.contains_key(&stage));
    assert!(!store.gc(Duration::ZERO).unwrap().blocked_by_staging);
    assert!(store.read_object(&object).is_ok());
}

#[test]
fn expired_missing_and_corrupt_inputs_leave_staging_and_legacy_schema_unchanged() {
    let (_temp, store) = fixture();
    let object = store.put_blob(b"local").unwrap();
    let expired = store.begin_staging(Duration::from_millis(1)).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    assert!(store
        .retain_object_set(&expired.id, "expired", &[object.clone()])
        .is_err());
    let stage = lease(&store);
    let missing = ObjectRef::from_bytes(ObjectKind::Blob, b"absent");
    assert!(store
        .retain_object_set(&stage, "missing", &[missing])
        .is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            store.object_path(&object).unwrap(),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    fs::write(store.object_path(&object).unwrap(), b"wrong").unwrap();
    assert!(store
        .retain_object_set(&stage, "corrupt", &[object])
        .is_err());
    let state = store.retention().unwrap();
    assert_eq!(state.format_version, 1);
    assert!(state.staging.contains_key(&stage));
    assert!(!store.root().join("object-sets.sqlite").exists());
}

#[test]
fn missing_or_corrupt_ledger_aborts_before_any_sweep() {
    let (_temp, store) = fixture();
    let stage = lease(&store);
    let object = store.put_blob(b"retained").unwrap();
    store
        .retain_object_set(&stage, "retain", &[object])
        .unwrap();
    let orphan = store.put_blob(b"would otherwise sweep").unwrap();
    let ledger = store.root().join("object-sets.sqlite");
    fs::rename(&ledger, store.root().join("displaced.sqlite")).unwrap();
    assert!(store.gc(Duration::ZERO).is_err());
    assert!(LocalStore::open(store.root()).is_err());
    assert!(store.object_path(&orphan).unwrap().exists());
    fs::rename(store.root().join("displaced.sqlite"), &ledger).unwrap();
    let connection = rusqlite::Connection::open(&ledger).unwrap();
    connection
        .execute("UPDATE object_sets SET refs='[]'", [])
        .unwrap();
    drop(connection);
    assert!(store.gc(Duration::ZERO).is_err());
    assert!(store.object_path(&orphan).unwrap().exists());
}

#[test]
fn individual_pins_and_complete_pins_coexist_under_concurrent_collection() {
    let (_temp, store) = fixture();
    let stage = lease(&store);
    let object = store.put_blob(b"unit pin").unwrap();
    let complete = store.put_blob(b"complete pin").unwrap();
    store.pin("complete", &complete).unwrap();
    let other = store.clone();
    let collector = std::thread::spawn(move || {
        for _ in 0..10 {
            other.gc(Duration::ZERO).unwrap();
        }
    });
    let receipt = store
        .retain_object_set(&stage, "individual", &[object.clone()])
        .unwrap();
    collector.join().unwrap();
    assert_eq!(store.retained_closure(&complete).unwrap(), vec![complete]);
    assert!(store.read_object(&object).is_ok());
    store.release_object_set(&receipt).unwrap();
    store.gc(Duration::ZERO).unwrap();
    assert!(store.read_object(&object).is_err());
}

#[test]
fn object_set_crash_child() {
    let Some(path) = std::env::var_os("RUDDER_OBJECT_SET_CRASH_ROOT") else {
        return;
    };
    #[cfg(unix)]
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &limit);
    }
    let path = std::path::PathBuf::from(path);
    let store = LocalStore::open(path.join("cas")).unwrap();
    let stage = lease(&store);
    let object = store
        .put_blob(b"acknowledged before process abort")
        .unwrap();
    let receipt = store.retain_object_set(&stage, "crash", &[object]).unwrap();
    let receipt_path = path.join("receipt.json");
    fs::write(&receipt_path, canonical_json(&receipt).unwrap()).unwrap();
    fs::File::open(&receipt_path).unwrap().sync_all().unwrap();
    fs::File::open(&path).unwrap().sync_all().unwrap();
    std::process::abort();
}

#[test]
fn acknowledged_individual_objects_survive_unclean_process_exit() {
    let temp = tempfile::tempdir().unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "object_set_crash_child"])
        .env("RUDDER_OBJECT_SET_CRASH_ROOT", temp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!child.success());
    let receipt: ObjectSetReceipt =
        serde_json::from_slice(&fs::read(temp.path().join("receipt.json")).unwrap()).unwrap();
    let store = LocalStore::open(temp.path().join("cas")).unwrap();
    let objects = store.object_set_refs(&receipt).unwrap();
    assert!(store.retention().unwrap().staging.is_empty());
    store.gc(Duration::ZERO).unwrap();
    assert_eq!(
        store.read_object(&objects[0]).unwrap(),
        b"acknowledged before process abort"
    );
    let state: RetentionState = store.retention().unwrap();
    assert!(
        state.pins.is_empty(),
        "individual durability never became complete-root authority"
    );
}
