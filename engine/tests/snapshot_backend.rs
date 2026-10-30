use rudder_engine::{
    protocol::{canonical_json, CheckpointConsistency, ObjectKind, ObjectRef},
    snapshot::{self, EnvironmentV2, RunnableRoot, SourceV2},
    store::{DirectoryEntry, DirectoryManifest, EntryNode, EnvironmentManifest, LocalStore, SourceManifest},
};
use std::{collections::BTreeMap, time::Duration};

fn root(source: ObjectRef, environment: Option<ObjectRef>, directory: ObjectRef, profile: ObjectRef, hardlinks: ObjectRef) -> RunnableRoot {
    RunnableRoot { format_version: 1, consistency: CheckpointConsistency::Quiesced, source, environment, view_root: directory, profile, hardlinks, knowledge_cursor: u64::MAX }
}
fn directory(store: &LocalStore, file: Option<ObjectRef>) -> ObjectRef {
    store.put_directory(DirectoryManifest { format_version: 1, mode: 0o755, entries: file.map(|object| DirectoryEntry::new(b"file", EntryNode::File { object, mode: 0o644 }).unwrap()).into_iter().collect() }).unwrap()
}

#[test]
fn launch_closure_is_independent_of_retained_old_history_and_gc_preserves_each_pin() {
    let tmp = tempfile::tempdir().unwrap(); let store = LocalStore::open(tmp.path()).unwrap();
    let lease = store.begin_staging(Duration::from_secs(3600)).unwrap();
    let old_bytes = store.put_blob(b"old source retained by history, not by launch").unwrap();
    let old_dir = directory(&store, Some(old_bytes.clone()));
    let old_source = store.put_source(&SourceManifest { format_version: 1, consistency: CheckpointConsistency::Quiesced, directory: old_dir, parents: vec![] }).unwrap();
    store.pin("history:old", &old_source).unwrap();
    let current = directory(&store, None);
    let policy = store.put_blob(b"{\"projection\":\"source-v1\"}").unwrap();
    let profile = store.put_blob(b"{\"profile\":\"test\"}").unwrap();
    let hardlinks = store.put_blob(b"{\"format_version\":1,\"groups\":[]}").unwrap();
    let source = snapshot::stage(&store, &lease.id, ObjectKind::SourceRoot, &SourceV2 { format_version: 2, consistency: CheckpointConsistency::Quiesced, directory: current.clone(), projection_policy: policy.clone() }).unwrap();
    let mut last = None;
    for cursor in 0..12 {
        let mut manifest = root(source.clone(), None, current.clone(), profile.clone(), hardlinks.clone());
        manifest.knowledge_cursor = cursor;
        let reference = snapshot::stage(&store, &lease.id, ObjectKind::RunnableRoot, &manifest).unwrap();
        let closure = store.closure(&reference).unwrap();
        assert_eq!(closure.len(), 6, "retaining more history must not expand the launch closure");
        assert!(!closure.contains(&old_source) && !closure.contains(&old_bytes));
        store.pin(&format!("history:launch:{cursor}"), &reference).unwrap();
        last = Some(reference);
    }
    let current_root = last.unwrap();
    store.publish(&lease.id, "launch:current", &current_root).unwrap();
    store.gc(Duration::ZERO).unwrap();
    assert!(store.read_object(&old_bytes).is_ok());
    store.unpin("history:old").unwrap();
    assert!(store.gc(Duration::ZERO).unwrap().removed_objects >= 2);
    assert!(store.read_object(&old_bytes).is_err());
    assert_eq!(store.retained_closure(&current_root).unwrap().len(), 6);
    assert!(store.retention().unwrap().staging.is_empty());
}

#[test]
fn retained_legacy_source_or_environment_cannot_smuggle_history_into_a_runnable_root() {
    let tmp = tempfile::tempdir().unwrap(); let store = LocalStore::open(tmp.path()).unwrap();
    let lease = store.begin_staging(Duration::from_secs(3600)).unwrap();
    let dir = directory(&store, None); let blob = store.put_blob(b"{}").unwrap();
    let old_source = store.put_source(&SourceManifest { format_version: 1, consistency: CheckpointConsistency::Quiesced, directory: dir.clone(), parents: vec![] }).unwrap();
    let old_env = store.put_environment(&EnvironmentManifest { format_version: 1, directory: dir.clone(), platform: BTreeMap::from([("os".into(), "linux".into())]), installer_inputs: vec![old_source.clone()], provenance: BTreeMap::new() }).unwrap();
    store.pin("old-source", &old_source).unwrap(); store.pin("old-environment", &old_env).unwrap();
    let source = snapshot::stage(&store, &lease.id, ObjectKind::SourceRoot, &SourceV2 { format_version: 2, consistency: CheckpointConsistency::Quiesced, directory: dir.clone(), projection_policy: blob.clone() }).unwrap();
    for manifest in [root(old_source.clone(), None, dir.clone(), blob.clone(), blob.clone()), root(source.clone(), Some(old_env.clone()), dir.clone(), blob.clone(), blob.clone())] {
        let reference = snapshot::stage(&store, &lease.id, ObjectKind::RunnableRoot, &manifest).unwrap();
        assert!(store.publish(&lease.id, "forbidden", &reference).is_err());
        assert!(store.publish_extending(&lease.id, "forbidden-retained", &reference, &[old_source.clone(), old_env.clone()]).is_err());
        assert!(!store.retention().unwrap().pins.contains_key("forbidden"));
    }
    let environment = EnvironmentV2 { format_version: 2, directory: dir.clone(), platform: BTreeMap::from([("os".into(), "linux".into())]), installer_inputs: vec![dir.clone(), blob.clone()], provenance: BTreeMap::new() };
    let env = snapshot::stage(&store, &lease.id, ObjectKind::EnvironmentRoot, &environment).unwrap();
    let reference = snapshot::stage(&store, &lease.id, ObjectKind::RunnableRoot, &root(source, Some(env), dir, blob.clone(), blob)).unwrap();
    store.publish(&lease.id, "current", &reference).unwrap();
    let closure = store.retained_closure(&reference).unwrap();
    assert!(!closure.contains(&old_source) && !closure.contains(&old_env));
}

#[test]
fn ancestry_fields_history_installer_inputs_and_false_consistency_fail_before_staging() {
    let reference = ObjectRef::from_bytes(ObjectKind::Directory, b"directory");
    let blob = ObjectRef::from_bytes(ObjectKind::Blob, b"{}");
    let source = SourceV2 { format_version: 2, consistency: CheckpointConsistency::Quiesced, directory: reference.clone(), projection_policy: blob.clone() };
    let mut value = serde_json::to_value(&source).unwrap(); value["parents"] = serde_json::json!([]);
    assert!(snapshot::decode_source(&canonical_json(&value).unwrap()).is_err());
    value = serde_json::to_value(&source).unwrap(); value["consistency"] = "best_effort".into();
    assert!(snapshot::decode_source(&canonical_json(&value).unwrap()).is_err());
    for kind in [ObjectKind::SourceRoot, ObjectKind::EnvironmentRoot, ObjectKind::Checkpoint, ObjectKind::RunnableRoot, ObjectKind::ActionKey, ObjectKind::ActionResult, ObjectKind::DirectoryShard] {
        let mut history = blob.clone(); history.kind = kind;
        let env = EnvironmentV2 { format_version: 2, directory: reference.clone(), platform: BTreeMap::from([("os".into(), "linux".into())]), installer_inputs: vec![history], provenance: BTreeMap::new() };
        assert!(snapshot::decode_environment(&canonical_json(&env).unwrap()).is_err());
    }
    let manifest = root(ObjectRef::from_bytes(ObjectKind::SourceRoot, b"source"), None, reference, blob.clone(), blob);
    assert_eq!(snapshot::decode_runnable(&canonical_json(&manifest).unwrap()).unwrap().knowledge_cursor, u64::MAX);
    let mut value = serde_json::to_value(manifest).unwrap(); value["parents"] = serde_json::json!([]);
    assert!(snapshot::decode_runnable(&canonical_json(&value).unwrap()).is_err());
}
