#![cfg(unix)]

use rudder_engine::protocol::{
    canonical_json, CheckpointConsistency, Durability, ObjectKind, ObjectRef, FORMAT_VERSION,
};
use rudder_engine::store::{
    CheckpointManifest, DirectoryEntry, DirectoryManifest, EntryNode, EnvironmentManifest,
    ImportPolicy, LocalStore, ReferencedMetadata, SourceManifest,
};
use std::collections::BTreeMap;
use std::fs::{self, File, FileTimes};
use std::os::unix::{
    ffi::{OsStrExt, OsStringExt},
    fs::{symlink, PermissionsExt},
    net::UnixListener,
};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

fn directory(store: &LocalStore, entries: Vec<DirectoryEntry>) -> ObjectRef {
    store
        .put_directory(DirectoryManifest {
            format_version: FORMAT_VERSION,
            mode: 0o755,
            entries,
        })
        .unwrap()
}

fn file_entry(name: &[u8], object: ObjectRef) -> DirectoryEntry {
    DirectoryEntry::new(
        name,
        EntryNode::File {
            object,
            mode: 0o644,
        },
    )
    .unwrap()
}

fn source(store: &LocalStore, directory: ObjectRef) -> ObjectRef {
    store
        .put_source(&SourceManifest {
            format_version: FORMAT_VERSION,
            directory,
            parents: vec![],
            consistency: CheckpointConsistency::Quiesced,
        })
        .unwrap()
}

fn corrupt(store: &LocalStore, object: &ObjectRef, replacement: &[u8]) {
    let path = store.object_path(object).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(path, replacement).unwrap();
}

#[test]
fn standard_hashes_receive_checksums_and_durable_reopen() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path().join("cas")).unwrap();
    let object = store.put_blob(b"abc").unwrap();
    assert_eq!(
        object.digest,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    store.pin("source", &object).unwrap();
    drop(store);
    let reopened = LocalStore::open(tmp.path().join("cas")).unwrap();
    assert_eq!(reopened.read_object(&object).unwrap(), b"abc");
    assert_eq!(reopened.retention().unwrap().pins["source"], object);
    assert!(reopened.receive(&object, b"abd").is_err());
    assert!(reopened.receive(&object, b"abcd").is_err());
    assert_eq!(reopened.read_object(&object).unwrap(), b"abc");
    let mut malicious = object.clone();
    malicious.digest = "../escape".into();
    assert!(reopened.read_object(&malicious).is_err());
    assert!(reopened.receive(&malicious, b"abc").is_err());
}

#[test]
fn canonical_trees_are_order_independent_and_validate_unsafe_metadata() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let blob = store.put_blob(b"same content").unwrap();
    let a = file_entry(b"a", blob.clone());
    let z = file_entry(&[0xff], blob.clone());
    assert_eq!(
        directory(&store, vec![z.clone(), a.clone()]),
        directory(&store, vec![a.clone(), z])
    );
    let invalid = DirectoryManifest {
        format_version: FORMAT_VERSION,
        mode: 0o755,
        entries: vec![a.clone(), a],
    };
    assert!(store.put_directory(invalid).is_err());
    for name in [b"".as_slice(), b".", b"..", b"a/b", b"bad\0name"] {
        assert!(DirectoryEntry::new(
            name,
            EntryNode::File {
                object: blob.clone(),
                mode: 0o644
            }
        )
        .is_err());
    }
    let wrong_type = ObjectRef::from_bytes(ObjectKind::Directory, b"{}");
    assert!(DirectoryEntry::new(
        b"x",
        EntryNode::File {
            object: wrong_type,
            mode: 0o644
        }
    )
    .is_err());
    assert!(DirectoryEntry::new(
        b"x",
        EntryNode::File {
            object: blob,
            mode: 0o100644
        }
    )
    .is_err());
    for target in ["", "00", "GG"] {
        assert!(DirectoryEntry::new(
            b"link",
            EntryNode::Symlink {
                target_hex: target.into()
            }
        )
        .is_err());
    }
    let spaced = b"{ \"entries\":[],\"format_version\":1,\"mode\":493}";
    let reference = ObjectRef::from_bytes(ObjectKind::Directory, spaced);
    assert!(store.receive(&reference, spaced).is_err());
    let unknown =
        canonical_json(&serde_json::json!({ "format_version": 2, "mode": 493, "entries": [] }))
            .unwrap();
    assert!(store
        .receive(
            &ObjectRef::from_bytes(ObjectKind::Directory, &unknown),
            &unknown
        )
        .is_err());
}

#[test]
fn import_export_preserves_raw_names_modes_and_link_text_without_following_links() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input");
    fs::create_dir(&input).unwrap();
    fs::create_dir(input.join("sub")).unwrap();
    fs::write(input.join("sub/tool"), b"executable").unwrap();
    fs::set_permissions(input.join("sub/tool"), fs::Permissions::from_mode(0o751)).unwrap();
    fs::set_permissions(input.join("sub"), fs::Permissions::from_mode(0o711)).unwrap();
    fs::set_permissions(&input, fs::Permissions::from_mode(0o750)).unwrap();
    // APFS requires valid UTF-8 filenames. Keep physical Unicode coverage on
    // macOS; canonical_trees above still tests arbitrary raw metadata names.
    let raw_name = if cfg!(target_os = "macos") {
        std::ffi::OsString::from("ø-source")
    } else {
        std::ffi::OsString::from_vec(vec![0xff, b'a'])
    };
    fs::write(input.join(&raw_name), b"non-utf8").unwrap();
    let external = tmp.path().join("outside");
    fs::write(&external, b"must not be imported or overwritten").unwrap();
    symlink(&external, input.join("external-link")).unwrap();
    let raw_link = std::ffi::OsString::from_vec(vec![0xfe, b'x']);
    symlink(&raw_link, input.join("raw-link")).unwrap();
    let store = LocalStore::open(tmp.path().join("cas")).unwrap();
    let root = store
        .import_tree(&input, CheckpointConsistency::Quiesced)
        .unwrap();
    assert!(!store
        .closure(&root)
        .unwrap()
        .iter()
        .any(|reference| store.read_object(reference).unwrap()
            == b"must not be imported or overwritten"));
    let output = tmp.path().join("output");
    store.export_tree(&root, &output).unwrap();
    assert_eq!(fs::read(output.join(&raw_name)).unwrap(), b"non-utf8");
    assert_eq!(
        fs::read_link(output.join("external-link")).unwrap(),
        external
    );
    assert_eq!(
        fs::read_link(output.join("raw-link"))
            .unwrap()
            .as_os_str()
            .as_bytes(),
        raw_link.as_bytes()
    );
    assert_eq!(
        fs::metadata(&output).unwrap().permissions().mode() & 0o7777,
        0o750
    );
    assert_eq!(
        fs::metadata(output.join("sub"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o711
    );
    assert_eq!(
        fs::metadata(output.join("sub/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o751
    );
    assert_eq!(
        store
            .import_tree(&output, CheckpointConsistency::Quiesced)
            .unwrap(),
        root
    );
    fs::write(output.join("sub/tool"), b"private write").unwrap();
    assert_eq!(fs::read(input.join("sub/tool")).unwrap(), b"executable");
    let other = tmp.path().join("other");
    store.export_tree(&root, &other).unwrap();
    assert_eq!(fs::read(other.join("sub/tool")).unwrap(), b"executable");
    assert_eq!(
        fs::read(tmp.path().join("outside")).unwrap(),
        b"must not be imported or overwritten"
    );
}

#[test]
fn import_exclusions_are_explicit_and_default_keeps_git_data() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input");
    fs::create_dir_all(input.join(".git")).unwrap();
    fs::write(input.join(".git/config"), b"git config").unwrap();
    fs::write(input.join("main"), b"source").unwrap();
    let store = LocalStore::open(tmp.path().join("cas")).unwrap();
    let all = store
        .import_tree(&input, CheckpointConsistency::Quiesced)
        .unwrap();
    assert_eq!(store.read_directory(&all).unwrap().entries.len(), 2);
    let excluded = store
        .import_tree_with_policy(
            &input,
            CheckpointConsistency::Quiesced,
            &ImportPolicy {
                excluded_paths: vec![vec![b".git".to_vec()]],
            },
        )
        .unwrap();
    assert_eq!(store.read_directory(&excluded).unwrap().entries.len(), 1);
    assert!(input.join(".git/config").exists());
    for excluded_paths in [vec![vec![]], vec![vec![b"..".to_vec()]]] {
        assert!(store
            .import_tree_with_policy(
                &input,
                CheckpointConsistency::Quiesced,
                &ImportPolicy { excluded_paths }
            )
            .is_err());
    }
}

#[test]
fn persistent_updates_share_siblings_and_do_not_mutate_parent_or_sibling_views() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path().join("cas")).unwrap();
    let old = store.put_blob(b"old").unwrap();
    let child = directory(&store, vec![file_entry(b"file", old.clone())]);
    let unchanged = directory(&store, vec![file_entry(b"keep", old)]);
    let root = directory(
        &store,
        vec![
            DirectoryEntry::new(b"child", EntryNode::Directory { object: child }).unwrap(),
            DirectoryEntry::new(
                b"sibling",
                EntryNode::Directory {
                    object: unchanged.clone(),
                },
            )
            .unwrap(),
        ],
    );
    store.pin("base", &root).unwrap();
    store.pin_existing("sibling-view", &root).unwrap();
    let new_blob = store.put_blob(b"new").unwrap();
    let changed = store
        .update_path(
            &root,
            &[b"child".to_vec(), b"file".to_vec()],
            Some(file_entry(b"file", new_blob)),
        )
        .unwrap();
    assert_ne!(changed, root);
    let original = store.read_directory(&root).unwrap();
    let updated = store.read_directory(&changed).unwrap();
    assert_eq!(original.entries[1], updated.entries[1]);
    assert_eq!(
        updated.entries[1].node,
        EntryNode::Directory { object: unchanged }
    );
    for (name, reference, expected) in [
        ("parent", &root, b"old".as_slice()),
        ("sibling", &root, b"old"),
        ("changed", &changed, b"new"),
    ] {
        let destination = tmp.path().join(name);
        store.export_tree(reference, &destination).unwrap();
        assert_eq!(fs::read(destination.join("child/file")).unwrap(), expected);
    }
    let deleted = store
        .update_path(&changed, &[b"child".to_vec(), b"file".to_vec()], None)
        .unwrap();
    store
        .export_tree(&deleted, tmp.path().join("deleted"))
        .unwrap();
    assert!(!tmp.path().join("deleted/child/file").exists());
    assert_eq!(store.retention().unwrap().pins["sibling-view"], root);
    assert!(store.pin_existing("unknown", &changed).is_err());
    assert!(store.update_path(&root, &[b"..".to_vec()], None).is_err());
    assert!(store
        .update_path(
            &root,
            &[b"child".to_vec()],
            Some(file_entry(b"different", store.put_blob(b"x").unwrap()))
        )
        .is_err());
}

#[test]
fn updates_never_traverse_symlinks_and_exports_never_merge_with_existing_paths() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path().join("cas")).unwrap();
    let root = directory(
        &store,
        vec![DirectoryEntry::new(
            b"outside",
            EntryNode::Symlink {
                target_hex: hex::encode(b"../outside"),
            },
        )
        .unwrap()],
    );
    assert!(store
        .update_path(&root, &[b"outside".to_vec(), b"file".to_vec()], None)
        .is_err());
    let destination = tmp.path().join("existing");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("sentinel"), b"keep").unwrap();
    assert!(store.export_tree(&root, &destination).is_err());
    assert_eq!(fs::read(destination.join("sentinel")).unwrap(), b"keep");
    let alias = tmp.path().join("alias");
    symlink(&destination, &alias).unwrap();
    assert!(store.export_tree(&root, &alias).is_err());
    assert!(store
        .import_tree(&alias, CheckpointConsistency::Quiesced)
        .is_err());
}

#[test]
fn special_files_are_rejected_without_opening_or_following_them() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input");
    fs::create_dir(&input).unwrap();
    let _socket = UnixListener::bind(input.join("socket")).unwrap();
    let store = LocalStore::open(tmp.path().join("cas")).unwrap();
    assert!(store
        .import_tree(&input, CheckpointConsistency::Quiesced)
        .is_err());
    fs::remove_file(input.join("socket")).unwrap();
    let fifo = std::ffi::CString::new(input.join("fifo").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(store
        .import_tree(&input, CheckpointConsistency::Quiesced)
        .is_err());
}

#[test]
fn local_manifests_retain_complete_source_environment_checkpoint_and_action_closures() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let blob = store.put_blob(b"source").unwrap();
    let tree = directory(&store, vec![file_entry(b"main", blob.clone())]);
    let source_root = source(&store, tree.clone());
    let installer = store.put_blob(b"lockfile").unwrap();
    let environment = store
        .put_environment(&EnvironmentManifest {
            format_version: FORMAT_VERSION,
            directory: tree,
            platform: BTreeMap::from([
                ("os".into(), "linux".into()),
                ("arch".into(), "x86_64".into()),
            ]),
            installer_inputs: vec![installer.clone()],
            provenance: BTreeMap::from([("builder".into(), "test".into())]),
        })
        .unwrap();
    let checkpoint_manifest = CheckpointManifest {
        format_version: FORMAT_VERSION,
        source: source_root.clone(),
        environment: Some(environment.clone()),
        parents: vec![],
        consistency: CheckpointConsistency::Quiesced,
        durability: Durability::Local,
        knowledge_cursor: 42,
    };
    let checkpoint = store.put_checkpoint(&checkpoint_manifest).unwrap();
    let result = store
        .put_metadata(
            ObjectKind::ActionResult,
            &ReferencedMetadata {
                format_version: FORMAT_VERSION,
                references: vec![checkpoint.clone()],
                payload: serde_json::json!({"exit_code": 0}),
            },
        )
        .unwrap();
    let closure = store.closure(&result).unwrap();
    for reference in [
        &result,
        &checkpoint,
        &environment,
        &source_root,
        &installer,
        &blob,
    ] {
        assert!(closure.contains(reference));
    }
    let lease = store.begin_staging(Duration::from_secs(60)).unwrap();
    store.publish(&lease.id, "checkpoint", &result).unwrap();
    assert!(!store.gc(Duration::ZERO).unwrap().blocked_by_staging);
    assert!(store.read_object(&installer).is_ok());
    let mut invalid = checkpoint_manifest;
    invalid.durability = Durability::Replicated;
    assert!(store.put_checkpoint(&invalid).is_err());
}

#[test]
fn transfer_verifies_before_acceptance_and_publication_is_atomic_with_retention() {
    let tmp = TempDir::new().unwrap();
    let origin = LocalStore::open(tmp.path().join("origin")).unwrap();
    let remote = LocalStore::open(tmp.path().join("remote")).unwrap();
    let blob = origin.put_blob(b"remote bytes").unwrap();
    let tree = directory(&origin, vec![file_entry(b"main", blob)]);
    let root = source(&origin, tree);
    let lease = remote.begin_staging(Duration::from_secs(60)).unwrap();
    remote
        .receive(&root, &origin.read_object(&root).unwrap())
        .unwrap();
    let before = remote.retention().unwrap();
    assert!(remote.publish(&lease.id, "accepted", &root).is_err());
    assert_eq!(remote.retention().unwrap(), before);
    assert!(remote.gc(Duration::ZERO).unwrap().blocked_by_staging);
    for reference in origin.closure(&root).unwrap() {
        remote
            .receive(&reference, &origin.read_object(&reference).unwrap())
            .unwrap();
    }
    remote.publish(&lease.id, "accepted", &root).unwrap();
    let reopened = LocalStore::open(tmp.path().join("remote")).unwrap();
    assert_eq!(reopened.retention().unwrap().pins["accepted"], root);
    assert!(reopened.retention().unwrap().staging.is_empty());
    assert_eq!(
        reopened.closure(&root).unwrap(),
        origin.closure(&root).unwrap()
    );
    assert!(remote.publish(&lease.id, "replayed", &root).is_err());
}

#[test]
fn expired_staging_cannot_publish_and_active_staging_retains_unpublished_data() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let lease = store.begin_staging(Duration::from_secs(60)).unwrap();
    let blob = store.put_blob(b"in flight").unwrap();
    assert!(store.gc(Duration::ZERO).unwrap().blocked_by_staging);
    store
        .renew_staging(&lease.id, Duration::from_secs(120))
        .unwrap();
    store.abandon_staging(&lease.id).unwrap();
    let expired = store.begin_staging(Duration::from_millis(1)).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    assert!(store.publish(&expired.id, "must-fail", &blob).is_err());
    assert!(store
        .renew_staging(&expired.id, Duration::from_secs(60))
        .is_err());
    assert_eq!(store.gc(Duration::ZERO).unwrap().removed_objects, 1);
    assert!(store.read_object(&blob).is_err());
}

#[test]
fn gc_preserves_live_fork_roots_and_never_publishes_a_swept_object() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let kept = store.put_blob(b"kept").unwrap();
    let orphan = store.put_blob(b"orphan").unwrap();
    store.pin("base", &kept).unwrap();
    store.pin_existing("fork", &kept).unwrap();
    store.unpin("base").unwrap();
    let result = store.gc(Duration::ZERO).unwrap();
    assert_eq!(result.removed_objects, 1);
    assert!(store.read_object(&kept).is_ok());
    assert!(store.read_object(&orphan).is_err());
    assert!(store.pin("invalid", &orphan).is_err());
    store.unpin("fork").unwrap();
    assert!(store.pin_existing("too-late", &kept).is_err());
    assert_eq!(store.gc(Duration::ZERO).unwrap().removed_objects, 1);
}

#[test]
fn corrupted_objects_and_retention_fail_closed_before_gc_deletes_anything() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let root = store.put_blob(b"pinned").unwrap();
    store.pin("base", &root).unwrap();
    let orphan = store.put_blob(b"orphan").unwrap();
    corrupt(&store, &root, b"broken");
    assert!(store.read_object(&root).is_err());
    assert!(
        store.put_blob(b"pinned").is_err(),
        "put never silently repairs corrupted immutable bytes"
    );
    assert!(store.gc(Duration::ZERO).is_err());
    assert_eq!(store.read_object(&orphan).unwrap(), b"orphan");
    let state_path = tmp.path().join("retention.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    json["state"]["pins"] = serde_json::json!({}); // valid JSON, invalid checksum.
    fs::write(&state_path, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(store.gc(Duration::ZERO).is_err());
    assert!(LocalStore::open(tmp.path()).is_err());
    assert!(store.object_path(&orphan).unwrap().exists());
}

#[test]
fn interrupted_temp_writes_are_not_publications_and_missing_state_is_not_reset() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let blob = store.put_blob(b"committed").unwrap();
    store.pin("base", &blob).unwrap();
    fs::write(tmp.path().join("tmp/interrupted-object"), b"partial data").unwrap();
    fs::write(tmp.path().join("tmp/interrupted-state"), b"{\"state\":").unwrap();
    let reopened = LocalStore::open(tmp.path()).unwrap();
    assert_eq!(reopened.retention().unwrap().pins["base"], blob);
    assert_eq!(reopened.read_object(&blob).unwrap(), b"committed");
    reopened.gc(Duration::ZERO).unwrap();
    assert!(
        tmp.path().join("tmp/interrupted-object").exists(),
        "unknown temporary recovery data is conservatively retained"
    );
    fs::remove_file(tmp.path().join("retention.json")).unwrap();
    assert!(LocalStore::open(tmp.path()).is_err());
    assert!(store.object_path(&blob).unwrap().exists());
}

#[test]
fn staging_retention_survives_restart_and_commit_failure_does_not_drop_old_pins() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let old = store.put_blob(b"old retained root").unwrap();
    store.pin("base", &old).unwrap();
    let stage = store.begin_staging(Duration::from_secs(60)).unwrap();
    let pending = store.put_blob(b"acknowledged in-flight object").unwrap();
    let reopened = LocalStore::open(tmp.path()).unwrap();
    assert_eq!(reopened.retention().unwrap().staging[&stage.id], stage);
    assert!(reopened.gc(Duration::ZERO).unwrap().blocked_by_staging);
    assert_eq!(
        reopened.read_object(&pending).unwrap(),
        b"acknowledged in-flight object"
    );
    // A rename permission failure occurs after temp state has been written and
    // synced. Root processes bypass Unix permission checks, so that injection
    // is only meaningful for an ordinary user (the shipped worker runs as one).
    if unsafe { libc::geteuid() } != 0 {
        let before = reopened.retention().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = reopened.publish(&stage.id, "base", &pending);
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(reopened.retention().unwrap(), before);
        assert_eq!(reopened.retention().unwrap().pins["base"], old);
    }
    reopened.publish(&stage.id, "base", &pending).unwrap();
    assert_eq!(
        LocalStore::open(tmp.path())
            .unwrap()
            .retention()
            .unwrap()
            .pins["base"],
        pending
    );
}

#[test]
fn symlinked_store_paths_and_objects_are_rejected_without_following() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path().join("cas")).unwrap();
    let blob = store.put_blob(b"object").unwrap();
    let object_path = store.object_path(&blob).unwrap();
    fs::remove_file(&object_path).unwrap();
    let outside = tmp.path().join("outside");
    fs::write(&outside, b"object").unwrap();
    symlink(&outside, &object_path).unwrap();
    assert!(store.read_object(&blob).is_err());
    assert!(store.put_blob(b"object").is_err());
    assert!(store.gc(Duration::ZERO).is_err());
    assert_eq!(fs::read(outside).unwrap(), b"object");
    let alias = tmp.path().join("alias");
    symlink(store.root(), &alias).unwrap();
    assert!(LocalStore::open(alias).is_err());
}

#[test]
fn grace_and_future_timestamps_prevent_premature_collection() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let blob = store.put_blob(b"new").unwrap();
    assert_eq!(
        store.gc(Duration::from_secs(60)).unwrap().removed_objects,
        0
    );
    File::open(store.object_path(&blob).unwrap())
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(3600)))
        .unwrap();
    assert_eq!(
        store.gc(Duration::ZERO).unwrap().removed_objects,
        0,
        "future modification times are retained even with zero grace"
    );
    assert!(store.read_object(&blob).is_ok());
}

#[test]
fn independent_store_handles_serialize_concurrent_pin_updates() {
    let tmp = TempDir::new().unwrap();
    let store = LocalStore::open(tmp.path()).unwrap();
    let object = store.put_blob(b"shared").unwrap();
    store.pin("base", &object).unwrap();
    let handles: Vec<_> = (0..12)
        .map(|index| {
            let root = tmp.path().to_owned();
            let object = object.clone();
            std::thread::spawn(move || {
                LocalStore::open(root)
                    .unwrap()
                    .pin_existing(&format!("view-{index}"), &object)
                    .unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let state = store.retention().unwrap();
    assert_eq!(state.pins.len(), 13);
    assert_eq!(state.generation, 13);
}
